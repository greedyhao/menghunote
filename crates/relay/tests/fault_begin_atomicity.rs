//! 评审 0002 §五 T01/T02/T03：`begin` 在**每个 SQL 点**失败后的全有或全无。
//!
//! 做法：真实 `Fixture`（真 SQLite/WAL）+ 真实 Ed25519 管理签名 + 真实 axum HTTP 路由；
//! 用 SQLite TRIGGER 在指定表/语句上 `RAISE(ABORT)`，制造"事务执行到第 N 条 SQL 失败"。
//! 失败后断言：S / revocation / auth_rev / control / epoch 分配 / dedup 全部未变；
//! 撤销注入后用同一 request_id/body 重试成功，且旧 epoch 写不会因失败被解冻。
//!
//! 不 sleep；收敛点都是显式 HTTP 响应或 SQL 查询。

mod support;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use relay::api::{router, AppState};
use relay::ids::Id16;
use relay::verify::Ed25519Hook;
use relay::wire;
use support::*;
use tower::ServiceExt;

const REVOKED: Id16 = [0x99; 16];
const REQUEST_ID: Id16 = [0x1A; 16];

fn block<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}

fn app_ed25519(fx: &Fixture) -> axum::Router {
    let state = AppState::new(fx.db.inner().clone(), fx.cfg.clone(), Arc::new(Ed25519Hook));
    router(state)
}

async fn call(app: &axum::Router, request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    (status, bytes)
}

fn error_code(body: &[u8]) -> String {
    wire::text_field(&cbor_fields(body), 0).unwrap_or_default()
}

fn b64(bytes: &[u8]) -> String {
    relay::ids::b64url_encode(bytes)
}

fn begin_path(fx: &Fixture) -> String {
    format!("/v1/vaults/{}/rotations", b64(&fx.vault_id))
}

/// 带真实管理签名的 begin 请求（外层镜像与 canonical body 逐字节一致）。
///
/// `expected_epoch` / `control_rev` 显式传入：成功后再重放时必须复用**完全相同**的字节，
/// 不能从执行后的 vault 事实重新拼装（否则 request_id 相同而 body 不同，会被 409 拒绝）。
fn begin_request(
    fx: &Fixture,
    token: &str,
    request_id: Id16,
    expected_epoch: u64,
    control_rev: u64,
) -> Request<Body> {
    let body = cbor_body(vec![
        (0, wire::uint(expected_epoch)),
        (1, wire::uint(control_rev)),
        (2, wire::bytes(request_id)),
        (3, wire::array(vec![wire::bytes(REVOKED)])),
    ]);
    let signature = fx.sign_management(
        relay::proto::HttpMethod::Post,
        &begin_path(fx),
        request_id,
        control_rev,
        &body,
    );
    let outer = vec![
        (0, Fixture::signed_wrapper(body, signature)),
        (1, wire::bytes(request_id)),
        (2, wire::uint(control_rev)),
        (3, wire::uint(expected_epoch)),
        (4, wire::array(vec![wire::bytes(REVOKED)])),
    ];
    Request::builder()
        .method("POST")
        .uri(begin_path(fx))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, wire::CONTENT_TYPE)
        .body(Body::from(cbor_body(outer)))
        .expect("begin request")
}

/// `begin` 会触碰的全部业务事实（失败时逐项必须不变）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Facts {
    state: String,
    active_epoch: u64,
    next_epoch: u64,
    manager_rev: u64,
    control_rev: u64,
    auth_rev: u64,
    head_seq: u64,
    gc_floor: u64,
    rotations: i64,
    controls: i64,
    dedup: i64,
    revoked_status: String,
    epoch1_revoked_once: i64,
    epoch2_state: Option<String>,
    epochs: i64,
}

fn facts(fx: &Fixture) -> Facts {
    let vault = fx.vault();
    let (rotations, controls, dedup, epochs, revoked_status, epoch1_revoked_once) = fx
        .db
        .read(|conn| {
            let count =
                |sql: &str| -> rusqlite::Result<i64> { conn.query_row(sql, [], |row| row.get(0)) };
            let revoked: String = conn.query_row(
                "SELECT status FROM devices WHERE device_id = ?1",
                [REVOKED.as_slice()],
                |row| row.get(0),
            )?;
            let epoch1: i64 = conn.query_row(
                "SELECT revoked_once FROM epochs WHERE epoch = 1",
                [],
                |row| row.get(0),
            )?;
            Ok((
                count("SELECT COUNT(*) FROM rotations")?,
                count("SELECT COUNT(*) FROM control_commits")?,
                count("SELECT COUNT(*) FROM dedup")?,
                count("SELECT COUNT(*) FROM epochs")?,
                revoked,
                epoch1,
            ))
        })
        .expect("facts");
    let epoch2_state: Option<String> = fx
        .db
        .read(|conn| {
            Ok(conn
                .query_row("SELECT state FROM epochs WHERE epoch = 2", [], |row| {
                    row.get::<_, String>(0)
                })
                .ok())
        })
        .expect("epoch2");
    Facts {
        state: format!("{:?}", vault.state),
        active_epoch: vault.active_epoch,
        next_epoch: vault.next_epoch,
        manager_rev: vault.manager_rev,
        control_rev: vault.control_rev,
        auth_rev: vault.auth_rev,
        head_seq: vault.head_seq,
        gc_floor: vault.gc_floor,
        rotations,
        controls,
        dedup,
        revoked_status,
        epoch1_revoked_once,
        epoch2_state,
        epochs,
    }
}

fn install_trigger(fx: &Fixture, sql: &str) {
    let sql = sql.to_owned();
    fx.raw_write(move |conn| {
        conn.execute_batch(&sql)?;
        Ok(())
    })
    .expect("安装故障注入触发器");
}

fn drop_trigger(fx: &Fixture, name: &str) {
    let sql = format!("DROP TRIGGER {name}");
    fx.raw_write(move |conn| {
        conn.execute_batch(&sql)?;
        Ok(())
    })
    .expect("撤销故障注入触发器");
}

/// 每个注入点一条触发器；覆盖 begin 事务里的 epoch 分配、auth_rev、吊销、revoked_once、
/// rotation 行、状态切换、控制记录与幂等回执。
const FAILURE_POINTS: &[(&str, &str)] = &[
    (
        "fail_epoch_alloc",
        "CREATE TRIGGER fail_epoch_alloc BEFORE INSERT ON epochs \
         WHEN NEW.epoch > 1 BEGIN SELECT RAISE(ABORT,'injected epoch alloc'); END;",
    ),
    (
        "fail_auth_rev",
        "CREATE TRIGGER fail_auth_rev BEFORE UPDATE ON vaults \
         WHEN NEW.auth_rev <> OLD.auth_rev BEGIN SELECT RAISE(ABORT,'injected auth_rev'); END;",
    ),
    (
        "fail_revoke",
        "CREATE TRIGGER fail_revoke BEFORE UPDATE ON devices \
         WHEN NEW.status = 'revoked' BEGIN SELECT RAISE(ABORT,'injected revoke'); END;",
    ),
    (
        "fail_revoked_once",
        "CREATE TRIGGER fail_revoked_once BEFORE UPDATE ON epochs \
         WHEN NEW.revoked_once = 1 BEGIN SELECT RAISE(ABORT,'injected revoked_once'); END;",
    ),
    (
        "fail_rotation_insert",
        "CREATE TRIGGER fail_rotation_insert BEFORE INSERT ON rotations \
         BEGIN SELECT RAISE(ABORT,'injected rotation'); END;",
    ),
    (
        "fail_set_state",
        "CREATE TRIGGER fail_set_state BEFORE UPDATE ON vaults \
         WHEN NEW.state = 'rotating' BEGIN SELECT RAISE(ABORT,'injected state'); END;",
    ),
    (
        "fail_control",
        "CREATE TRIGGER fail_control BEFORE INSERT ON control_commits \
         BEGIN SELECT RAISE(ABORT,'injected control'); END;",
    ),
    (
        "fail_dedup",
        "CREATE TRIGGER fail_dedup BEFORE INSERT ON dedup \
         BEGIN SELECT RAISE(ABORT,'injected dedup'); END;",
    ),
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn begin_is_all_or_nothing_at_each_sql_failure_point() {
    let fx = block(|| Fixture::with_real_manager("fault-begin-sql"));
    let app = app_ed25519(&fx);
    block(|| fx.add_device(REVOKED));
    let initial = block(|| facts(&fx));

    for (name, sql) in FAILURE_POINTS {
        block(|| install_trigger(&fx, sql));
        let before = block(|| facts(&fx));
        let token = block(|| fx.manager_token());
        let (status, body) = call(
            &app,
            block(|| {
                begin_request(
                    &fx,
                    &token,
                    REQUEST_ID,
                    before.active_epoch,
                    before.control_rev,
                )
            }),
        )
        .await;
        assert!(
            !status.is_success(),
            "注入点 {name} 必须让 begin 失败，实际 {status} {}",
            error_code(&body)
        );
        assert_eq!(
            block(|| facts(&fx)),
            before,
            "注入点 {name} 失败后不得留下任何部分事实（S/revocation/auth_rev/control/epoch/dedup）"
        );
        block(|| drop_trigger(&fx, name));
    }

    // 全部注入点都证明"失败零效应"后，同一 request/body 必须成功。
    let expected_epoch = initial.active_epoch;
    let control_rev = initial.control_rev;
    let token = block(|| fx.manager_token());
    let (status, body) = call(
        &app,
        block(|| begin_request(&fx, &token, REQUEST_ID, expected_epoch, control_rev)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    let view = wire::value_map(&fields_of(&body)[0].1).expect("rotation view");
    let rotation_id = wire::id16_field(&view, 0).expect("rotation id");

    let after = block(|| facts(&fx));
    assert_eq!(
        after.rotations,
        initial.rotations + 1,
        "只允许新增一条 rotation"
    );
    assert_eq!(
        after.controls,
        initial.controls + 1,
        "只允许新增一条控制记录"
    );
    assert_eq!(after.dedup, initial.dedup + 1, "成功回执必须持久化幂等");
    assert_eq!(after.revoked_status, "revoked");
    assert_eq!(
        after.epoch1_revoked_once, 1,
        "吊销过的旧 epoch 必须留下永久事实"
    );
    assert_eq!(after.epoch2_state.as_deref(), Some("staged"));
    assert_eq!(after.epochs, initial.epochs + 1, "新候选 epoch 已分配");
    assert_eq!(after.active_epoch, initial.active_epoch, "activate 前不变");
    assert_eq!(after.next_epoch, initial.next_epoch + 1);
    assert_eq!(after.state, "Rotating");
    assert_eq!(
        after.manager_rev, initial.manager_rev,
        "begin 不推进管理 key rev"
    );
    assert_eq!(
        after.control_rev,
        initial.control_rev + 1,
        "begin 追加一条控制记录"
    );
    assert_eq!(
        after.auth_rev,
        initial.auth_rev + 2,
        "freeze 与吊销各推进一次 auth_rev，旧 token 全部失效"
    );
    assert_eq!(after.head_seq, initial.head_seq, "S 固定为冻结水位");

    // exact request 重放：同一 request_id/body 原样回放，不产生第二条 rotation/控制记录。
    let token = block(|| fx.manager_token());
    let (status, replayed) = call(
        &app,
        block(|| begin_request(&fx, &token, REQUEST_ID, expected_epoch, control_rev)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&replayed));
    assert_eq!(replayed, body, "同一 request_id/body 必须回放原始响应字节");
    assert_eq!(
        wire::id16_field(
            &wire::value_map(&fields_of(&replayed)[0].1).expect("replayed view"),
            0
        )
        .expect("rotation id"),
        rotation_id
    );
    let replayed_facts = block(|| facts(&fx));
    assert_eq!(replayed_facts.rotations, initial.rotations + 1);
    assert_eq!(replayed_facts.controls, initial.controls + 1);
    assert_eq!(replayed_facts.dedup, initial.dedup + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_begin_does_not_freeze_old_epoch_writes() {
    let fx = block(|| Fixture::with_real_manager("fault-begin-no-freeze"));
    let app = app_ed25519(&fx);
    block(|| fx.add_device(REVOKED));
    let initial = block(|| facts(&fx));

    // 在事务最后一步（控制记录）失败：此前已 bump auth_rev / 吊销 / 分配 epoch，全部必须回滚。
    block(|| {
        install_trigger(
            &fx,
            "CREATE TRIGGER fail_control BEFORE INSERT ON control_commits \
             BEGIN SELECT RAISE(ABORT,'injected control'); END;",
        )
    });
    let token = block(|| fx.manager_token());
    let (status, body) = call(
        &app,
        block(|| {
            begin_request(
                &fx,
                &token,
                REQUEST_ID,
                initial.active_epoch,
                initial.control_rev,
            )
        }),
    )
    .await;
    assert!(!status.is_success(), "{}", error_code(&body));
    block(|| drop_trigger(&fx, "fail_control"));

    // 失败后旧 epoch 仍然可写（没有解冻问题，因为事务回滚后根本没冻结）。
    let object_id = random_object_id();
    let envelope = support::single_part_envelope_at(
        relay::envelope::ObjectType::Update,
        fx.vault_id,
        object_id,
        1,
        1,
        32,
        1,
    );
    block(|| fx.put_part(&envelope, None)).expect("失败事务不得冻结旧 epoch 写");
    block(|| fx.complete_object(&object_id, &envelope)).expect("失败事务不得冻结旧 epoch complete");
    let head_after_write = block(|| fx.head_seq());
    assert_eq!(head_after_write, initial.head_seq + 1);

    // 再 begin 成功：freeze_seq 必须是当前 head（包含刚才旧写），revocation/auth_rev/control 才生效。
    let token = block(|| fx.manager_token());
    let (status, body) = call(
        &app,
        block(|| {
            begin_request(
                &fx,
                &token,
                REQUEST_ID,
                initial.active_epoch,
                initial.control_rev,
            )
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    assert_eq!(
        block(|| fx.rotation_view(&rotation_id_of(&body)))
            .rotation
            .freeze_seq,
        head_after_write
    );
    assert_eq!(
        block(|| fx.vault()).state,
        relay::vault::VaultState::Rotating
    );
    assert_eq!(block(|| fx.vault()).auth_rev, initial.auth_rev + 2);
    assert_eq!(block(|| facts(&fx)).revoked_status, "revoked");
    assert_eq!(block(|| facts(&fx)).epoch1_revoked_once, 1);
}

fn fields_of(body: &[u8]) -> wire::Fields {
    cbor_fields(body)
}

fn rotation_id_of(body: &[u8]) -> Id16 {
    wire::id16_field(
        &wire::value_map(&fields_of(body)[0].1).expect("rotation view"),
        0,
    )
    .expect("rotation id")
}
