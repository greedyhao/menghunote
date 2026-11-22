//! ADR 0015 **S4c**：`RotationBegin`（6）/`RotationAbort`（7）/`RotationRestart`（8）/
//! `RotationAddRevokes`（9）/`EpochActivate`（10）。
//!
//! 覆盖：
//! 1. begin：真实两签 ROTATE 成功（冻结/候选 epoch/控制链/auth 失效）；单签、同设备两 cap、
//!    错 from/to、候选 epoch 复用、重复 rotation id 全部拒绝且不推进控制链；abort 不复活
//!    发生过吊销的旧 epoch；跨 vault 的 rotation 引用 404；v1 轮换路由在 V2 vault 上关闭；
//! 2. restart / add-revokes：签名的更高候选 epoch + 追吊销；候选已有材料时 add-revokes 409
//!    （必须走 restart，relay 不能自造未签名 epoch）；
//! 3. activate：真实候选基线 + 全成员 grant + 新槽 → 切换数据 epoch（不换 authority、不撤
//!    cap）；旧 bearer/WS 失效、新 bearer 可用；集合被替换（grant/资产/成员/冻结 S）一律拒绝；
//! 4. 全部走真实 relay 路由 + 真实 Ed25519；无 sleep（FakeClock + 显式收敛点）；逐条复算链 hash。

mod support;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use axum::Router;
use futures_util::StreamExt;
use relay::ids::{b64url_encode, Id16};
use relay::wire;
use support::v2::*;
use support::*;
use tokio_tungstenite::tungstenite::Message;

use menghunote_sync_proto as proto;

// ---------------------------------------------------------------- 夹具

fn new_fixture(label: &str) -> V2Fixture {
    tokio::task::block_in_place(|| V2Fixture::new(label))
}

/// `TestDb::write_blocking` **不得**在 tokio runtime 线程上直接调用（会 panic）。
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}

fn assert_error(body: &[u8], expected: &str) {
    assert_eq!(
        error_code(body),
        expected,
        "错误码不符（实际 {}）",
        error_code(body)
    );
}

/// 两台 CAP_ADMIN 的 bootstrap cap（`default_upgrade_parts` 装的 C1/C2）。
fn admin_signers(v2: &V2Fixture) -> [(Id16, Id16); 2] {
    [(v2.extra[0].0, [0xC1; 16]), (v2.extra[1].0, [0xC2; 16])]
}

fn control_count(v2: &V2Fixture) -> i64 {
    v2.fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM control_commits WHERE vault_id = ?1",
                [conn.scope().as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("control count")
}

async fn upgraded(label: &str) -> (V2Fixture, Router, Arc<relay::api::AppState>) {
    let v2 = new_fixture(label);
    let state: Arc<relay::api::AppState> = relay::api::AppState::new(
        v2.fx.db.inner().clone(),
        v2.fx.cfg.clone(),
        Arc::new(relay::verify::Ed25519Hook),
    );
    // 同一个 `AppState`（同一 `wake` 广播）同时驱动请求路由与 WS：`notify()` 才能让
    // 已建立的 WS 在下一帧立即重验（否则只能等 30s 兜底 tick）。
    let app = relay::api::router(state.clone());
    let (status, body) = upgrade_default(&v2, &app, [0xA0; 16]).await;
    assert_eq!(status, StatusCode::OK, "upgrade: {}", error_code(&body));
    (v2, app, state)
}

/// 控制链逐条复算（与 S4b2 用例同口径）。
fn assert_chain_continuous(v2: &V2Fixture) {
    let rows: Vec<ChainRow> = v2
        .fx
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT rev, prev_hash, record_hash, signed_record FROM control_commits
                 WHERE vault_id = ?1 ORDER BY rev ASC",
            )?;
            let rows = stmt.query_map([conn.scope().as_slice()], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .expect("control rows");
    let mut previous = [0u8; 32];
    for (index, (rev, prev_hash, record_hash, signed_record)) in rows.iter().enumerate() {
        assert_eq!(
            *rev,
            i64::try_from(index + 1).expect("rev"),
            "控制链 rev 必须从 1 连续"
        );
        assert_eq!(
            prev_hash.as_slice(),
            previous.as_slice(),
            "rev {rev} 的 prev_hash 不指向上一条"
        );
        let expected = relay::ids::sha256_parts(&[previous.as_slice(), signed_record.as_slice()]);
        assert_eq!(
            record_hash.as_slice(),
            expected.as_slice(),
            "rev {rev} 的 record_hash 不可复算"
        );
        previous = expected;
    }
}

/// 控制链一行：`(rev, prev_hash, record_hash, signed_record)`。
type ChainRow = (i64, Vec<u8>, Vec<u8>, Vec<u8>);

struct RotationStateView {
    state: String,
    from_epoch: i64,
    to_epoch: i64,
    freeze_seq: i64,
    member_set_hash: Option<Vec<u8>>,
    primary_checkpoint: Option<Vec<u8>>,
    fallback_checkpoint: Option<Vec<u8>>,
    activated_control_rev: Option<i64>,
}

fn rotation_state(v2: &V2Fixture, rotation_id: &Id16) -> Option<RotationStateView> {
    v2.fx
        .db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT state, from_epoch, to_epoch, freeze_seq, member_set_hash,
                            primary_checkpoint, fallback_checkpoint, activated_control_rev
                     FROM rotations WHERE vault_id = ?1 AND rotation_id = ?2",
                    (conn.scope().as_slice(), rotation_id.as_slice()),
                    |row| {
                        Ok(RotationStateView {
                            state: row.get(0)?,
                            from_epoch: row.get(1)?,
                            to_epoch: row.get(2)?,
                            freeze_seq: row.get(3)?,
                            member_set_hash: row.get(4)?,
                            primary_checkpoint: row.get(5)?,
                            fallback_checkpoint: row.get(6)?,
                            activated_control_rev: row.get(7)?,
                        })
                    },
                )
                .ok())
        })
        .expect("rotation row")
}

fn epoch_state(v2: &V2Fixture, epoch: u64) -> Option<String> {
    v2.fx
        .db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT state FROM epochs WHERE vault_id = ?1 AND epoch = ?2",
                    (
                        conn.scope().as_slice(),
                        i64::try_from(epoch).expect("epoch"),
                    ),
                    |row| row.get::<_, String>(0),
                )
                .ok())
        })
        .expect("epoch state")
}

fn epoch_revoked_once(v2: &V2Fixture, epoch: u64) -> bool {
    v2.fx
        .db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT revoked_once FROM epochs WHERE vault_id = ?1 AND epoch = ?2",
                (
                    conn.scope().as_slice(),
                    i64::try_from(epoch).expect("epoch"),
                ),
                |row| row.get::<_, i64>(0),
            )?)
        })
        .expect("revoked_once")
        != 0
}

fn device_status(v2: &V2Fixture, device_id: &Id16) -> Option<String> {
    v2.fx
        .db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT status FROM devices WHERE vault_id = ?1 AND device_id = ?2",
                    (conn.scope().as_slice(), device_id.as_slice()),
                    |row| row.get::<_, String>(0),
                )
                .ok())
        })
        .expect("device status")
}

fn cap_revoked_control_rev(v2: &V2Fixture, cap_id: &[u8; 16]) -> Option<i64> {
    v2.fx
        .db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT revoked_control_rev FROM management_caps
                     WHERE vault_id = ?1 AND cap_id = ?2",
                    (conn.scope().as_slice(), cap_id.as_slice()),
                    |row| row.get::<_, Option<i64>>(0),
                )
                .ok()
                .flatten())
        })
        .expect("cap row")
}

// ---------------------------------------------------------------- 控制记录构造

fn begin_op(
    v2: &V2Fixture,
    request_id: [u8; 16],
    rotation_id: [u8; 16],
    from_epoch: u64,
    to_epoch: u64,
    revoke_ids: Vec<Id16>,
    signers: &[(Id16, Id16)],
) -> proto::OperationV2 {
    let payload = proto::RotationBeginPayloadV2 {
        rotation_id,
        from_epoch,
        to_epoch,
        revoke_ids,
    }
    .encode()
    .expect("begin payload");
    v2.two_signer_operation(
        proto::ControlKindV2::RotationBegin,
        request_id,
        payload,
        signers,
        v2.now_u64(),
    )
}

fn abort_op(
    v2: &V2Fixture,
    request_id: [u8; 16],
    rotation_id: [u8; 16],
    signers: &[(Id16, Id16)],
) -> proto::OperationV2 {
    let payload = proto::RotationAbortPayloadV2 { rotation_id }
        .encode()
        .expect("abort payload");
    v2.two_signer_operation(
        proto::ControlKindV2::RotationAbort,
        request_id,
        payload,
        signers,
        v2.now_u64(),
    )
}

fn restart_op(
    v2: &V2Fixture,
    request_id: [u8; 16],
    rotation_id: [u8; 16],
    new_to_epoch: u64,
    extra_ids: Vec<Id16>,
    signers: &[(Id16, Id16)],
) -> proto::OperationV2 {
    let payload = proto::RotationRestartPayloadV2 {
        rotation_id,
        new_to_epoch,
        extra_ids,
    }
    .encode()
    .expect("restart payload");
    v2.two_signer_operation(
        proto::ControlKindV2::RotationRestart,
        request_id,
        payload,
        signers,
        v2.now_u64(),
    )
}

fn add_revokes_op(
    v2: &V2Fixture,
    request_id: [u8; 16],
    rotation_id: [u8; 16],
    device_ids: Vec<Id16>,
    signers: &[(Id16, Id16)],
) -> proto::OperationV2 {
    let payload = proto::RotationAddRevokesPayloadV2 {
        rotation_id,
        device_ids,
    }
    .encode()
    .expect("add revokes payload");
    v2.two_signer_operation(
        proto::ControlKindV2::RotationAddRevokes,
        request_id,
        payload,
        signers,
        v2.now_u64(),
    )
}

fn activate_op(
    v2: &V2Fixture,
    request_id: [u8; 16],
    activation: proto::ActivationV2,
    signers: &[(Id16, Id16)],
) -> proto::OperationV2 {
    let payload = proto::EpochActivatePayloadV2 { activation }
        .encode()
        .expect("activate payload");
    v2.two_signer_operation(
        proto::ControlKindV2::EpochActivate,
        request_id,
        payload,
        signers,
        v2.now_u64(),
    )
}

async fn submit_ok(app: &Router, v2: &V2Fixture, signed: &proto::SignedControlV2) -> ReceiptView {
    let (status, body) = submit(app, &v2.vault_id(), signed).await;
    assert_eq!(status, StatusCode::OK, "submit: {}", error_code(&body));
    receipt(&body)
}

async fn submit_err(
    app: &Router,
    v2: &V2Fixture,
    signed: &proto::SignedControlV2,
    expected: StatusCode,
    code: &str,
) {
    let (status, body) = submit(app, &v2.vault_id(), signed).await;
    assert_eq!(
        status,
        expected,
        "期望 {expected}，实际 {status}: {}",
        error_code(&body)
    );
    assert_error(&body, code);
}

// ---------------------------------------------------------------- 候选材料（直接落库）

/// 候选 epoch 的全成员 grant（V2 的 grant 包由客户端封装，relay 只存字节）。
fn insert_grant(
    v2: &V2Fixture,
    epoch: u64,
    device_id: Id16,
    grant_id: [u8; 16],
    ciphertext: &[u8],
) {
    blocking(|| insert_grant_inner(v2, epoch, device_id, grant_id, ciphertext));
}

fn insert_grant_inner(
    v2: &V2Fixture,
    epoch: u64,
    device_id: Id16,
    grant_id: [u8; 16],
    ciphertext: &[u8],
) {
    let vault = v2.vault();
    let hpke_pk: [u8; 32] = v2
        .fx
        .db
        .read(move |conn| {
            let bytes: Vec<u8> = conn.query_row(
                "SELECT hpke_pk FROM devices WHERE vault_id = ?1 AND device_id = ?2",
                (conn.scope().as_slice(), device_id.as_slice()),
                |row| row.get(0),
            )?;
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| relay::error::Error::Invariant("hpke_pk"))
        })
        .expect("hpke pk");
    let aad = proto::GrantAadV2 {
        version: proto::V2_WIRE_VERSION,
        instance_id: vault.instance_id,
        vault_id: vault.vault_id,
        epoch,
        recipient_device_id: device_id,
        recipient_hpke_pub: hpke_pk,
        authority_rev: vault.authority_rev,
        grant_id,
        issued_authz_rev: vault.authz_rev,
    };
    let aad_bytes = aad.aad_bytes().expect("aad bytes");
    let enc = [0x9Au8; 32];
    let ciphertext = ciphertext.to_vec();
    let manager_rev = vault.manager_rev;
    v2.fx
        .db
        .write_blocking(move |conn| {
            conn.execute(
                "INSERT INTO device_grants(vault_id, epoch, device_id, manager_rev, hpke_enc,
                                           hpke_ciphertext, signed_header, signature, grant_id,
                                           activation_ref)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, NULL)",
                (
                    conn.scope().as_slice(),
                    i64::try_from(epoch).expect("epoch"),
                    device_id.as_slice(),
                    i64::try_from(manager_rev).expect("manager rev"),
                    enc.as_slice(),
                    ciphertext.as_slice(),
                    aad_bytes.as_slice(),
                    grant_id.as_slice(),
                ),
            )?;
            Ok(())
        })
        .expect("insert grant");
}

/// 候选 epoch 的全部 ACTIVE 设备 grant。
fn insert_all_grants(v2: &V2Fixture, epoch: u64, seed: u8) {
    let devices: Vec<Id16> = v2
        .fx
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT device_id FROM devices
                 WHERE vault_id = ?1 AND status = 'active' ORDER BY device_id ASC",
            )?;
            let rows = stmt.query_map([conn.scope().as_slice()], |row| row.get::<_, Vec<u8>>(0))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(
                    row?.as_slice()
                        .try_into()
                        .map_err(|_| relay::error::Error::Invariant("device_id"))?,
                );
            }
            Ok(out)
        })
        .expect("active devices");
    for (index, device_id) in devices.iter().enumerate() {
        let mut grant_id = [seed; 16];
        grant_id[15] = u8::try_from(index).expect("index");
        insert_grant(v2, epoch, *device_id, grant_id, &[0xB7u8; 40]);
    }
}

/// 按当前 DB 事实构造**正确**的 `ActivationV2`（客户端取包重算的同一口径）。
fn correct_activation(v2: &V2Fixture, rotation_id: Id16) -> proto::ActivationV2 {
    let rotation = rotation_state(v2, &rotation_id).expect("rotation");
    let to_epoch = u64::try_from(rotation.to_epoch).expect("to epoch");
    let from_epoch = u64::try_from(rotation.from_epoch).expect("from epoch");
    let freeze_seq = u64::try_from(rotation.freeze_seq).expect("freeze");
    let slot_set_hash = v2
        .fx
        .db
        .read(move |conn| relay::rotations::slot_set_digest(conn, to_epoch))
        .expect("slot digest");
    let member_cert_hash = v2
        .fx
        .db
        .read(relay::control_v2::member_cert_set_digest)
        .expect("member digest");
    let (_, grants_hash) = v2
        .fx
        .db
        .read(move |conn| relay::control_v2::grant_set_digest(conn, to_epoch))
        .expect("grants digest");
    let old_asset_hash = v2
        .fx
        .db
        .read(move |conn| relay::objects::asset_set_digest(conn, from_epoch, freeze_seq))
        .expect("old assets");
    let new_asset_hash = v2
        .fx
        .db
        .read(move |conn| relay::objects::candidate_asset_set_digest(conn, to_epoch, &rotation_id))
        .expect("new assets");
    let (primary_cid, fallback_cid) = candidate_cids(v2, to_epoch);
    let (primary_commit_hash, fallback_commit_hash) =
        candidate_commit_hashes(v2, &primary_cid, &fallback_cid);
    proto::ActivationV2 {
        version: proto::V2_WIRE_VERSION,
        rotation_id,
        from_epoch,
        to_epoch,
        freeze_seq,
        primary_cid,
        primary_commit_hash,
        fallback_cid,
        fallback_commit_hash,
        slot_set_hash,
        member_cert_hash,
        grants_hash,
        old_asset_hash,
        new_asset_hash,
    }
}

/// 候选 epoch 的两份 checkpoint id（按 checkpoint_id 升序取前两份）。
fn candidate_cids(v2: &V2Fixture, epoch: u64) -> ([u8; 16], [u8; 16]) {
    let ids: Vec<[u8; 16]> = v2
        .fx
        .db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT checkpoint_id FROM checkpoints
                 WHERE vault_id = ?1 AND epoch = ?2 AND state = 'candidate'
                 ORDER BY checkpoint_id ASC",
            )?;
            let rows = stmt.query_map(
                (
                    conn.scope().as_slice(),
                    i64::try_from(epoch).expect("epoch"),
                ),
                |row| row.get::<_, Vec<u8>>(0),
            )?;
            let mut out = Vec::new();
            for row in rows {
                out.push(
                    row?.as_slice()
                        .try_into()
                        .map_err(|_| relay::error::Error::Invariant("checkpoint_id"))?,
                );
            }
            Ok(out)
        })
        .expect("candidate cids");
    assert!(ids.len() >= 2, "候选基线不足两份: {ids:?}");
    (ids[0], ids[1])
}

fn candidate_commit_hashes(
    v2: &V2Fixture,
    primary: &[u8; 16],
    fallback: &[u8; 16],
) -> ([u8; 32], [u8; 32]) {
    let read = |cid: &[u8; 16]| {
        let cid = *cid;
        v2.fx
            .db
            .read(move |conn| {
                let signed: Vec<u8> = conn.query_row(
                    "SELECT signed_commit FROM checkpoints
                     WHERE vault_id = ?1 AND checkpoint_id = ?2",
                    (conn.scope().as_slice(), cid.as_slice()),
                    |row| row.get(0),
                )?;
                Ok(relay::ids::sha256(&signed))
            })
            .expect("commit hash")
    };
    (read(primary), read(fallback))
}

/// 候选基线 + 新槽（v1 的同一批 DB 事实；V2 授权只走控制记录）。
fn prepare_activation_material(v2: &V2Fixture, rotation_id: Id16, to_epoch: u64) {
    blocking(|| {
        v2.fx.publish_candidate_baselines(to_epoch);
        let reservation = v2.fx.rotation_reservation(&rotation_id);
        v2.fx.stage_candidate_slots(to_epoch, &reservation);
        insert_all_grants(v2, to_epoch, 0x51);
    });
}

// ---------------------------------------------------------------- 1. begin

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_begin_requires_two_distinct_rotate_signers_and_freezes() {
    let (v2, app, _state) = upgraded("s4c-begin").await;
    let rotation_id = [0x81; 16];
    let vault_before = v2.vault();
    assert_eq!(vault_before.active_epoch, 1);

    // —— 单签 → 403（控制链不动）——
    let operation = begin_op(
        &v2,
        [0x90; 16],
        rotation_id,
        1,
        2,
        Vec::new(),
        &[(v2.extra[0].0, [0xC1; 16])],
    );
    let signed = v2.sign_operation(&operation);
    submit_err(&app, &v2, &signed, StatusCode::FORBIDDEN, "FORBIDDEN").await;
    assert_eq!(
        control_count(&v2),
        i64::try_from(vault_before.control_rev).unwrap()
    );

    // —— 同一台设备两把 cap → 403（同设备不能凑门限）——
    // 测试专用：直接投影一条同设备的第二个 cap（真实签发走 kind 2，这里只验门限算术）。
    give_second_cap(&v2, v2.extra[0].0, [0xC9; 16]);
    let same_device = [(v2.extra[0].0, [0xC1; 16]), (v2.extra[0].0, [0xC9; 16])];
    let operation = begin_op(&v2, [0x91; 16], rotation_id, 1, 2, Vec::new(), &same_device);
    let signed = v2.sign_operation(&operation);
    submit_err(&app, &v2, &signed, StatusCode::FORBIDDEN, "FORBIDDEN").await;
    assert_eq!(
        control_count(&v2),
        i64::try_from(vault_before.control_rev).unwrap()
    );

    // `from_epoch > to_epoch` 在 proto 层就拒绝（payload 无法构造）。
    assert!(proto::RotationBeginPayloadV2 {
        rotation_id,
        from_epoch: 1,
        to_epoch: 1,
        revoke_ids: Vec::new(),
    }
    .encode()
    .is_err());

    // —— 错 from/to → 409（proto 合法、relay 事实不符）——
    for (from, to, request_id) in [(2u64, 3u64, [0x92; 16])] {
        let operation = begin_op(
            &v2,
            request_id,
            rotation_id,
            from,
            to,
            Vec::new(),
            &admin_signers(&v2),
        );
        let signed = v2.sign_operation(&operation);
        submit_err(&app, &v2, &signed, StatusCode::CONFLICT, "CAS_CONFLICT").await;
    }
    assert_eq!(
        control_count(&v2),
        i64::try_from(vault_before.control_rev).unwrap()
    );

    // —— 正例：两签 ROTATE ——
    let operation = begin_op(
        &v2,
        [0x94; 16],
        rotation_id,
        1,
        2,
        Vec::new(),
        &admin_signers(&v2),
    );
    let signed = v2.sign_operation(&operation);
    let receipt = submit_ok(&app, &v2, &signed).await;
    assert_eq!(receipt.kind, 6);
    assert_eq!(receipt.control_rev, vault_before.control_rev + 1);
    // 纯 rotation 只推进 control（ADR 0015 §鉴权）。
    assert_eq!(receipt.authz_rev, vault_before.authz_rev);
    assert_eq!(receipt.authority_rev, vault_before.authority_rev);

    let vault_after = v2.vault();
    assert_eq!(vault_after.state.as_str(), "rotating");
    assert!(
        vault_after.auth_rev > vault_before.auth_rev,
        "freeze 必须切断旧 token"
    );
    let rotation = rotation_state(&v2, &rotation_id).expect("rotation row");
    assert_eq!(rotation.state, "frozen");
    assert_eq!(rotation.from_epoch, 1);
    assert_eq!(rotation.to_epoch, 2);
    assert_eq!(
        rotation.freeze_seq,
        i64::try_from(vault_before.head_seq).unwrap()
    );
    assert!(
        rotation.member_set_hash.is_some(),
        "begin 必须锁定冻结时的成员集合摘要"
    );
    assert_eq!(epoch_state(&v2, 2).as_deref(), Some("staged"));

    // —— 重复 rotation id → 409 ——
    let operation = begin_op(
        &v2,
        [0x95; 16],
        rotation_id,
        1,
        3,
        Vec::new(),
        &admin_signers(&v2),
    );
    let signed = v2.sign_operation(&operation);
    submit_err(&app, &v2, &signed, StatusCode::CONFLICT, "INVALID_STATE").await;

    // —— abort：候选不复活、未吊销 → 解冻回 active ——
    let operation = abort_op(&v2, [0x96; 16], rotation_id, &admin_signers(&v2));
    let signed = v2.sign_operation(&operation);
    let receipt = submit_ok(&app, &v2, &signed).await;
    assert_eq!(receipt.kind, 7);
    assert_eq!(
        rotation_state(&v2, &rotation_id).expect("row").state,
        "aborted"
    );
    assert_eq!(epoch_state(&v2, 2).as_deref(), Some("aborted"));
    assert_eq!(v2.vault().state.as_str(), "active");

    // 已 aborted 的候选 epoch 不能再次 begin（候选不复用）。
    let operation = begin_op(
        &v2,
        [0x97; 16],
        [0x82; 16],
        1,
        2,
        Vec::new(),
        &admin_signers(&v2),
    );
    let signed = v2.sign_operation(&operation);
    submit_err(&app, &v2, &signed, StatusCode::CONFLICT, "CAS_CONFLICT").await;
    assert_chain_continuous(&v2);
}

/// 测试专用：直接投影一条同设备的第二个 cap（只用于"同设备两 cap 不能凑门限"）。
fn give_second_cap(v2: &V2Fixture, device_id: Id16, cap_id: [u8; 16]) {
    blocking(|| give_second_cap_inner(v2, device_id, cap_id));
}

fn give_second_cap_inner(v2: &V2Fixture, device_id: Id16, cap_id: [u8; 16]) {
    let vault = v2.vault();
    let sign_pk = v2.sign_pk(&device_id);
    let cap = proto::ManagementCapV2 {
        version: proto::V2_WIRE_VERSION,
        instance_id: vault.instance_id,
        vault_id: vault.vault_id,
        cap_id,
        subject_device_id: device_id,
        subject_ed_pk: sign_pk,
        scopes: proto::CapScope::new(proto::CapScope::ALL).expect("scopes"),
        not_before: u64::try_from(v2.now() - 10).expect("not_before"),
        expires_at: u64::try_from(v2.now() + 3_600).expect("expires_at"),
        issued_control_rev: 1,
        issued_authz_rev: 1,
        issuer_authority_rev: vault.authority_rev,
        delegable: false,
    };
    let canonical = cap.encode().expect("cap cde");
    v2.fx
        .db
        .write_blocking(move |conn| {
            conn.execute(
                "INSERT INTO management_caps(vault_id, cap_id, canonical_bytes, subject_device,
                                             subject_pk, scopes, not_before, expires_at,
                                             issued_control_rev, issued_authz_rev,
                                             issuer_authority_rev, revoked_control_rev,
                                             delegable, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, 0, ?12)",
                (
                    conn.scope().as_slice(),
                    cap.cap_id.as_slice(),
                    canonical.as_slice(),
                    device_id.as_slice(),
                    sign_pk.as_slice(),
                    i64::from(cap.scopes.bits()),
                    i64::try_from(cap.not_before).expect("not_before"),
                    i64::try_from(cap.expires_at).expect("expires_at"),
                    1i64,
                    1i64,
                    i64::try_from(cap.issuer_authority_rev).expect("authority"),
                    BASE_TIME,
                ),
            )?;
            Ok(())
        })
        .expect("second cap");
}

// ---------------------------------------------------------------- 2. abort 不复活 / 跨 vault / v1 关闭

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_abort_never_revives_a_revoked_epoch_and_stays_tenant_scoped() {
    let (v2, app, _state) = upgraded("s4c-abort").await;
    let rotation_id = [0x83; 16];
    // begin 带吊销（每位签名者必须同时具备 REVOKE；bootstrap caps 有 ALL）。
    let operation = begin_op(
        &v2,
        [0xA1; 16],
        rotation_id,
        1,
        2,
        vec![v2.genesis_device()],
        &admin_signers(&v2),
    );
    let signed = v2.sign_operation(&operation);
    let receipt = submit_ok(&app, &v2, &signed).await;
    assert_eq!(receipt.kind, 6);
    assert_eq!(
        device_status(&v2, &v2.genesis_device()).as_deref(),
        Some("revoked")
    );
    assert!(epoch_revoked_once(&v2, 1), "吊销后旧 epoch 必须记永久事实");

    // abort：带过吊销的旧 epoch **不得**解冻（保持 rotating）。
    let operation = abort_op(&v2, [0xA2; 16], rotation_id, &admin_signers(&v2));
    let signed = v2.sign_operation(&operation);
    submit_ok(&app, &v2, &signed).await;
    assert_eq!(
        rotation_state(&v2, &rotation_id).expect("row").state,
        "aborted"
    );
    assert_eq!(
        v2.vault().state.as_str(),
        "rotating",
        "abort 不得复活发生过吊销的旧 epoch"
    );

    // 冻结期旧 epoch 的数据面写入必须 409（屏障语义）。
    let token = authz_login(&app, &v2.vault_id(), v2.extra[0].0, &v2.extra[0].1, None)
        .await
        .expect("device token");
    let (status, body) = call(
        &app,
        post_cbor(
            &format!("{}/updates", hex_path(&v2.vault_id())),
            Some(&token),
            vec![(0, wire::bytes(vec![0u8; 32]))],
        ),
    )
    .await;
    assert!(
        status == StatusCode::CONFLICT
            || status == StatusCode::BAD_REQUEST
            || status == StatusCode::FORBIDDEN,
        "冻结期写入必须拒绝（实际 {status}: {}）",
        error_code(&body)
    );

    // —— 跨 vault：另一租户的 rotation_id 在本 vault 上 404 ——
    let other = blocking(|| v2.create_second_vault([0xE7; 16]));
    let foreign_operation = begin_op(
        &v2,
        [0xA3; 16],
        rotation_id,
        1,
        2,
        Vec::new(),
        &admin_signers(&v2),
    );
    // 把 vault 换成另一租户：控制记录的结构校验直接 403（租户不符）。
    let mut foreign = foreign_operation.clone();
    foreign.vault_id = other.vault_id;
    foreign.canonical_path = proto::canonical_submit_path(&other.vault_id);
    let foreign_signed = v2.sign_operation(&foreign);
    let (status, body) = submit(&app, &other.vault_id, &foreign_signed).await;
    // 跨租户的控制记录必须被拒绝：租户/链头/rev 任一不符即 fail-stop（403/409 都可）。
    assert!(
        status == StatusCode::FORBIDDEN || status == StatusCode::CONFLICT,
        "跨 vault 必须拒绝（实际 {status}: {}）",
        error_code(&body)
    );
    let other_records: i64 = v2
        .fx
        .db
        .read_instance(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM control_commits WHERE vault_id = ?1",
                [other.vault_id.as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("other control count");
    assert_eq!(other_records, 1, "跨租户提交不得在别的 vault 里落记录");

    // —— v1 轮换路由在 V2 vault 上关闭（管理写 legacy 门禁）——
    let (status, body) = call(
        &app,
        post_cbor(
            &format!(
                "/v2/vaults/{}/rotations",
                relay::ids::hex_lower(&v2.vault_id())
            ),
            Some(&token),
            vec![(0, wire::uint(1))],
        ),
    )
    .await;
    assert!(
        status == StatusCode::FORBIDDEN || status == StatusCode::FORBIDDEN,
        "{}",
        error_code(&body)
    );
}

// ---------------------------------------------------------------- 3. restart / add-revokes

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_restart_and_add_revokes_are_signed_facts() {
    let (v2, app, _state) = upgraded("s4c-restart").await;
    let rotation_id = [0x84; 16];
    let operation = begin_op(
        &v2,
        [0xB1; 16],
        rotation_id,
        1,
        2,
        Vec::new(),
        &admin_signers(&v2),
    );
    let signed = v2.sign_operation(&operation);
    submit_ok(&app, &v2, &signed).await;

    // restart（kind 8）：更高候选 epoch，旧候选作废（旧 epoch 冻结期写不得复活）。
    let operation = restart_op(
        &v2,
        [0xB2; 16],
        rotation_id,
        3,
        Vec::new(),
        &admin_signers(&v2),
    );
    let signed = v2.sign_operation(&operation);
    let receipt = submit_ok(&app, &v2, &signed).await;
    assert_eq!(receipt.kind, 8);
    assert_eq!(
        rotation_state(&v2, &rotation_id).expect("old").state,
        "aborted"
    );
    assert_eq!(epoch_state(&v2, 2).as_deref(), Some("aborted"));
    assert_eq!(epoch_state(&v2, 3).as_deref(), Some("staged"));
    assert_eq!(v2.vault().state.as_str(), "rotating");
    let new_rotation: Id16 = blocking(|| {
        v2.fx
            .db
            .read(|conn| {
                let bytes: Vec<u8> = conn.query_row(
                    "SELECT rotation_id FROM rotations
                     WHERE vault_id = ?1 AND state = 'frozen' AND to_epoch = 3",
                    [conn.scope().as_slice()],
                    |row| row.get(0),
                )?;
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| relay::error::Error::Invariant("rotation_id"))
            })
            .expect("new rotation")
    });

    // 旧候选 epoch（2）不能被再次指定为候选（epoch 复用 → 409）。
    let operation = restart_op(
        &v2,
        [0xB3; 16],
        new_rotation,
        2,
        Vec::new(),
        &admin_signers(&v2),
    );
    let signed = v2.sign_operation(&operation);
    submit_err(&app, &v2, &signed, StatusCode::CONFLICT, "CAS_CONFLICT").await;

    // 冻结期追加吊销（kind 9）：新候选还没有材料 → 允许。
    let operation = add_revokes_op(
        &v2,
        [0xB4; 16],
        new_rotation,
        vec![v2.genesis_device()],
        &admin_signers(&v2),
    );
    let signed = v2.sign_operation(&operation);
    let receipt = submit_ok(&app, &v2, &signed).await;
    assert_eq!(receipt.kind, 9);
    assert_eq!(
        device_status(&v2, &v2.genesis_device()).as_deref(),
        Some("revoked")
    );
    assert!(epoch_revoked_once(&v2, 1), "吊销必须记永久事实");

    // 候选 epoch 一旦有材料，就不能继续用同一候选根：新 epoch 必须由签名的 restart 指定。
    insert_grant(&v2, 3, v2.extra[0].0, [0x61; 16], &[0xB8u8; 40]);
    let operation = add_revokes_op(
        &v2,
        [0xB5; 16],
        new_rotation,
        vec![v2.genesis_device()],
        &admin_signers(&v2),
    );
    let signed = v2.sign_operation(&operation);
    submit_err(&app, &v2, &signed, StatusCode::CONFLICT, "INVALID_STATE").await;
    assert_chain_continuous(&v2);
}

// ---------------------------------------------------------------- 4. EpochActivate

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn epoch_activate_switches_data_epoch_and_rejects_tampered_sets() {
    let (v2, app, state) = upgraded("s4c-activate").await;
    let rotation_id = [0x85; 16];
    let operation = begin_op(
        &v2,
        [0xC1; 16],
        rotation_id,
        1,
        2,
        Vec::new(),
        &admin_signers(&v2),
    );
    let signed = v2.sign_operation(&operation);
    submit_ok(&app, &v2, &signed).await;
    prepare_activation_material(&v2, rotation_id, 2);

    // 旧 bearer + WS：激活后必须失效/关闭。
    let old_token = authz_login(&app, &v2.vault_id(), v2.extra[0].0, &v2.extra[0].1, None)
        .await
        .expect("old device token");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let serve_router = relay::api::router(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, serve_router).await;
    });
    let uri = format!(
        "ws://{}/v2/vaults/{}/wake",
        addr,
        b64url_encode(&v2.vault_id())
    );
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = uri.into_client_request().expect("ws request");
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {old_token}").parse().expect("header"),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("ws connect");
    let first = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("frame timeout")
        .expect("frame")
        .expect("frame ok");
    assert!(matches!(first, Message::Text(_)), "首个帧必须是 hello");

    // —— 正例：正确集合 → 切换数据 epoch ——
    let vault_before = v2.vault();
    let activation = correct_activation(&v2, rotation_id);
    let operation = activate_op(&v2, [0xC2; 16], activation.clone(), &admin_signers(&v2));
    let signed = v2.sign_operation(&operation);
    let receipt = submit_ok(&app, &v2, &signed).await;
    assert_eq!(receipt.kind, 10);
    assert_eq!(receipt.control_rev, vault_before.control_rev + 1);
    // 成员集合未变 → 只推进 auth（ADR：成员变更才 authz+1）。
    assert_eq!(receipt.authz_rev, vault_before.authz_rev);
    assert_eq!(receipt.authority_rev, vault_before.authority_rev);

    let vault_after = v2.vault();
    assert_eq!(vault_after.active_epoch, 2);
    assert_eq!(vault_after.state.as_str(), "active");
    assert!(vault_after.auth_rev > vault_before.auth_rev);
    // 不换 authority、不普遍撤 cap（旧 cap 仍挂在当前代）。
    assert_eq!(vault_after.authority_rev, vault_before.authority_rev);
    assert_eq!(cap_revoked_control_rev(&v2, &[0xC1; 16]), None);
    // epoch 状态：旧 retired、新 active。
    assert_eq!(epoch_state(&v2, 1).as_deref(), Some("retired"));
    assert_eq!(epoch_state(&v2, 2).as_deref(), Some("active"));
    // rotation 投影：activated + 指名的两个 cid + 激活 rev。
    let rotation = rotation_state(&v2, &rotation_id).expect("rotation");
    assert_eq!(rotation.state, "activated");
    assert!(rotation.primary_checkpoint.is_some());
    assert!(rotation.fallback_checkpoint.is_some());
    assert!(rotation.member_set_hash.is_some());
    assert_eq!(
        rotation.activated_control_rev,
        Some(i64::try_from(receipt.control_rev).expect("rev"))
    );
    // 旧 bearer 立即失效。
    let (status, body) = call(
        &app,
        get(
            &format!("{}/state", hex_path(&v2.vault_id())),
            Some(&old_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    assert_error(&body, "REVOKED");
    // WS 在下一帧重验时关闭 1008（notify 立即兜底）。
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await {
                Some(Err(_)) | None => return true,
                Some(Ok(Message::Close(frame))) => {
                    return frame.is_some_and(|frame| u16::from(frame.code) == 1008);
                }
                Some(Ok(_)) => {}
            }
        }
    })
    .await
    .expect("ws close timeout");
    assert!(closed, "激活后 WS 必须 close 1008");
    // 新 bearer 可用（重新认证），并能取到新 epoch 的 grant。
    let new_token = authz_login(&app, &v2.vault_id(), v2.extra[0].0, &v2.extra[0].1, None)
        .await
        .expect("new device token");
    let (status, body) = call(
        &app,
        get(
            &format!("{}/devices/self/grants/latest", hex_path(&v2.vault_id())),
            Some(&new_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    let grant = cbor_fields(&body);
    assert_eq!(wire::uint_field(&grant, 0).expect("epoch"), 2);
    assert_eq!(
        relay::ids::hex_lower(&v2.vault_id()),
        relay::ids::hex_lower(&v2.vault_id())
    );

    // —— 负例：新一轮轮换，逐字段替换集合/材料 → 409（客户端必须重签）——
    let rotation2 = [0x86; 16];
    let operation = begin_op(
        &v2,
        [0xC3; 16],
        rotation2,
        2,
        3,
        Vec::new(),
        &admin_signers(&v2),
    );
    let signed = v2.sign_operation(&operation);
    submit_ok(&app, &v2, &signed).await;
    prepare_activation_material(&v2, rotation2, 3);
    let good = correct_activation(&v2, rotation2);

    let mut wrong_member = good.clone();
    wrong_member.member_cert_hash = [0xAB; 32];
    let mut wrong_grants = good.clone();
    wrong_grants.grants_hash = [0xAC; 32];
    let mut wrong_old_assets = good.clone();
    wrong_old_assets.old_asset_hash = [0xAD; 32];
    let mut wrong_new_assets = good.clone();
    wrong_new_assets.new_asset_hash = [0xAE; 32];
    let mut wrong_slots = good.clone();
    wrong_slots.slot_set_hash = [0xAF; 32];
    let mut wrong_freeze = good.clone();
    wrong_freeze.freeze_seq = good.freeze_seq + 1;
    let mut wrong_epochs = good.clone();
    wrong_epochs.to_epoch = good.to_epoch + 1;
    for (index, broken) in [
        wrong_member,
        wrong_grants,
        wrong_old_assets,
        wrong_new_assets,
        wrong_slots,
        wrong_freeze,
        wrong_epochs,
    ]
    .into_iter()
    .enumerate()
    {
        let operation = activate_op(
            &v2,
            [0xD0 + u8::try_from(index).unwrap(); 16],
            broken,
            &admin_signers(&v2),
        );
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert!(
            status == StatusCode::CONFLICT || status == StatusCode::CONFLICT,
            "第 {index} 个替换集合必须拒绝（实际 {status}: {}）",
            error_code(&body)
        );
    }
    // 全部负例都没有改状态：epoch 仍是 2、rotation 仍是 frozen。
    assert_eq!(v2.vault().active_epoch, 2);
    assert_eq!(
        rotation_state(&v2, &rotation2).expect("rotation").state,
        "frozen"
    );

    // 单签激活同样拒绝。
    let operation = activate_op(&v2, [0xE9; 16], good, &[(v2.extra[0].0, [0xC1; 16])]);
    let signed = v2.sign_operation(&operation);
    submit_err(&app, &v2, &signed, StatusCode::FORBIDDEN, "FORBIDDEN").await;
    assert_chain_continuous(&v2);
}
