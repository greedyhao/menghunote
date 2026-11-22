//! ADR 0018 **S2**：预留（reservation）/claim/cancel 原子语义 + 只读列表/详情。
//!
//! 覆盖（任务书 §3.2–3.5）：
//! 1. 预留：CSPRNG `vault_id`、registry `RESERVED`、票据 `created_at/expires_at` 在写事务内、
//!    `quota=1`、**不建** `vaults` 行；同 vault 重复预留/终态 ID 复用拒绝；
//! 2. claim：同一事务完成票据消费、`RESERVED→ACTIVE` CAS、genesis/租户行、兄弟票据失效；
//!    错/过期/已用/取消票 403 且不部分写；`ACTIVE` → 409 且不消费票据；
//! 3. claim vs cancel / 双 claim 并发：恰好一个赢；取消赢不建 vault；claim 赢后 cancel 冲突；
//! 4. 列表/详情：区分 RESERVED/ACTIVE/EXPIRED（读时推导，不写库），只读不迁移。
//!
//! 全部用临时文件库 + in-process axum（`Router::oneshot`）+ `FakeClock`，**无 sleep**。

mod support;

use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use relay::api::{router, AppState};
use relay::ids::sha256;
use relay::registry::{self, CancelOutcome, LifecycleState};
use relay::time::Clock;
use relay::vault::{self, InitRequest};
use relay::wire;
use support::*;
use tower::ServiceExt;

const VAULT_X: [u8; 16] = [0xB7u8; 16];
const VAULT_Y: [u8; 16] = [0xB8u8; 16];
const VAULT_Z: [u8; 16] = [0xB9u8; 16];
const DEVICE_X: [u8; 16] = [0x0Bu8; 16];

/// 在 tokio 多线程 runtime 里执行同步 DB 调用（writer 线程 + `block_in_place`）。
fn block<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}

/// 默认关闭 v2 限流的夹具。
fn fixture(label: &str) -> Fixture {
    Fixture::with_v2_rate_limits(label, None, None)
}

fn app(fx: &Fixture) -> axum::Router {
    router(AppState::new(
        fx.db.inner().clone(),
        fx.cfg.clone(),
        fx.hook.clone(),
    ))
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

fn post_cbor(uri: &str, fields: wire::Fields) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, wire::CONTENT_TYPE)
        .body(Body::from(cbor_body(fields)))
        .expect("request")
}

fn error_code(body: &[u8]) -> String {
    wire::text_field(&cbor_fields(body), 0).unwrap_or_default()
}

fn init_request(vault_id: [u8; 16], device_id: [u8; 16], manager_pub: [u8; 32]) -> InitRequest {
    let sign_pk = [0x11u8; 32];
    let hpke_pk = [4u8; 32];
    InitRequest {
        vault_id,
        device_id,
        sign_pk,
        hpke_pk,
        manager_pub,
        setup_secret: String::new(),
        certificate: certificate_bytes(&vault_id, &device_id, &sign_pk, &hpke_pk, 1),
        certificate_signature: b"certificate-sig".to_vec(),
    }
}

fn create_request(ticket: &[u8], request: &InitRequest) -> Request<Body> {
    let fields = vec![
        (0, wire::bytes(ticket.to_vec())),
        (1, wire::bytes(request.vault_id.to_vec())),
        (2, wire::bytes(request.device_id.to_vec())),
        (3, wire::bytes(request.sign_pk.to_vec())),
        (4, wire::bytes(request.hpke_pk.to_vec())),
        (5, wire::bytes(request.manager_pub.to_vec())),
        (6, wire::bytes(request.certificate.clone())),
        (7, wire::bytes(request.certificate_signature.clone())),
    ];
    post_cbor("/v2/vaults", fields)
}

fn reserve(fx: &Fixture, vault_id: [u8; 16]) -> [u8; 32] {
    block(|| {
        relay::admin::create_reservation_for(fx.db.inner(), vault_id, Duration::from_secs(900))
            .expect("reserve")
            .token
    })
}

fn registry_row(fx: &Fixture, vault_id: [u8; 16]) -> Option<registry::RegistryRow> {
    fx.db
        .read_instance(move |conn| registry::load_raw(conn, &vault_id))
        .expect("registry")
}

fn ticket_row(fx: &Fixture, ticket: &[u8]) -> relay::admin::CreateTicketRow {
    relay::admin::load_create_ticket(fx.db.inner(), ticket)
        .expect("load ticket")
        .expect("ticket row")
}

fn vault_exists(fx: &Fixture, vault_id: [u8; 16]) -> bool {
    fx.db
        .inner()
        .read(relay::VaultScope::new(vault_id), vault::load_opt)
        .expect("load vault")
        .is_some()
}

fn vault_count(fx: &Fixture) -> i64 {
    fx.db
        .read_instance(|conn| {
            Ok(conn.query_row("SELECT COUNT(*) FROM vaults", [], |row| row.get(0))?)
        })
        .expect("count vaults")
}

fn dump(fx: &Fixture, table: &str) -> Vec<String> {
    fx.db
        .read_instance(|conn| {
            let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
            let cols: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<std::result::Result<_, _>>()?;
            let expression = cols
                .iter()
                .map(|column| format!("quote({column})"))
                .collect::<Vec<_>>()
                .join("||'|'||");
            let mut stmt = conn.prepare(&format!("SELECT {expression} FROM {table} ORDER BY 1"))?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .expect("dump table")
}

// ---------------------------------------------------------------- 1) 预留

#[test]
fn reservation_is_random_atomic_and_creates_no_vault_row() {
    let fx = fixture("registry-reservation");
    let before = fx.clock.now_secs();
    let issued = block(|| {
        relay::admin::create_reservation(fx.db.inner(), Duration::from_secs(900)).expect("reserve")
    });
    assert_eq!(issued.created_at, before);
    assert_eq!(issued.expires_at, before + 900);
    assert_eq!(issued.quota, 1);
    assert_ne!(issued.vault_id, [0u8; 16]);
    // registry RESERVED（created_at 在写事务内生成）。
    let row = registry_row(&fx, issued.vault_id).expect("registry row");
    assert_eq!(row.state, LifecycleState::Reserved);
    assert_eq!(row.created_at, before);
    assert_eq!(row.deleted_at, None);
    // 票据：created_at/expires_at/quota 同事务。
    let ticket = ticket_row(&fx, &issued.token);
    assert_eq!(ticket.vault_id, issued.vault_id);
    assert_eq!(ticket.created_at, before);
    assert_eq!(ticket.expires_at, before + 900);
    assert_eq!(ticket.quota, 1);
    assert_eq!(ticket.consumed_at, None);
    assert_eq!(ticket.token_hash, sha256(&issued.token));
    // 不建 vaults 行。
    assert!(!vault_exists(&fx, issued.vault_id));
    assert_eq!(vault_count(&fx), 1, "只有夹具的 legacy vault A");
    // 再预留：CSPRNG 产生不同 id。
    let second = block(|| {
        relay::admin::create_reservation(fx.db.inner(), Duration::from_secs(60)).expect("reserve")
    });
    assert_ne!(second.vault_id, issued.vault_id);
    assert_eq!(
        registry_row(&fx, second.vault_id).expect("row").state,
        LifecycleState::Reserved
    );
    // 同 vault 重复预留 / 终态 ID 复用拒绝。
    assert_eq!(
        block(|| relay::admin::create_reservation_for(
            fx.db.inner(),
            issued.vault_id,
            Duration::from_secs(60)
        ))
        .expect_err("duplicate")
        .code(),
        Some(relay::ErrorCode::CasConflict)
    );
    assert_eq!(
        block(|| relay::admin::create_reservation_for(
            fx.db.inner(),
            fx.vault_id,
            Duration::from_secs(60)
        ))
        .expect_err("active vault")
        .code(),
        Some(relay::ErrorCode::CasConflict)
    );
}

// ---------------------------------------------------------------- 2) claim

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claim_activates_registry_genesis_and_invalidates_siblings() {
    let fx = block(|| fixture("registry-claim"));
    let app = app(&fx);
    let ticket = block(|| reserve(&fx, VAULT_X));
    // 兄弟票据（同 vault，模拟旧库多票）：claim 必须把它一并作废。
    let sibling = [0x5Au8; 32];
    block(|| {
        fx.db.inner().write_instance_blocking(move |conn| {
            conn.execute(
                "INSERT INTO create_tickets(token_hash, vault_id, created_at, expires_at, quota,
                                            consumed_at)
                 VALUES (?1, ?2, ?3, ?4, 1, NULL)",
                (
                    sibling.as_slice(),
                    VAULT_X.as_slice(),
                    BASE_TIME,
                    BASE_TIME + 900,
                ),
            )?;
            Ok(())
        })
    })
    .expect("seed sibling");

    let init = init_request(VAULT_X, DEVICE_X, fx.manager_pub);
    let (status, body) = call(&app, create_request(&ticket, &init)).await;
    assert_eq!(status, StatusCode::CREATED, "body={:?}", error_code(&body));

    // registry ACTIVE + vaults 行 + 控制链/epoch/genesis 一致。
    let row = registry_row(&fx, VAULT_X).expect("registry row");
    assert_eq!(row.state, LifecycleState::Active);
    assert_eq!(row.created_at, BASE_TIME, "预留时刻是生命周期起点");
    let vault = fx
        .db
        .inner()
        .read(relay::VaultScope::new(VAULT_X), vault::load)
        .expect("vault");
    assert_eq!(vault.vault_id, VAULT_X);
    assert_eq!(vault.control_rev, 1);
    assert_eq!(vault.active_epoch, 1);
    let control: i64 = fx
        .db
        .read_instance(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM control_commits WHERE vault_id = ?1",
                [VAULT_X.as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("control count");
    assert_eq!(control, 1, "genesis 控制链必须与 registry 同事务落库");
    // 票据：被 claim 的一次用完；兄弟票失效。
    let claimed = block(|| ticket_row(&fx, &ticket));
    assert_eq!(claimed.quota, 0);
    assert_eq!(claimed.consumed_at, Some(BASE_TIME));
    let sibling_row: (i64, Option<i64>) = fx
        .db
        .read_instance(|conn| {
            Ok(conn.query_row(
                "SELECT quota, consumed_at FROM create_tickets WHERE token_hash = ?1",
                [sibling.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .expect("sibling row");
    assert_eq!(sibling_row, (0, Some(BASE_TIME)), "兄弟票据必须失效");
    assert_eq!(vault_count(&fx), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_or_non_reserved_tickets_are_rejected_without_partial_writes() {
    let fx = block(|| fixture("registry-claim-errors"));
    let app = app(&fx);
    let init_x = init_request(VAULT_X, DEVICE_X, fx.manager_pub);
    let ticket = block(|| reserve(&fx, VAULT_X));

    // 未知票据 → 403，不建库。
    let (status, body) = call(&app, create_request(&[0x99u8; 32], &init_x)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "FORBIDDEN");
    assert!(!vault_exists(&fx, VAULT_X));

    // 错 vault：票据绑定 X，请求要建 Y → 403，票据不消费。
    let init_y = init_request(VAULT_Y, DEVICE_X, fx.manager_pub);
    let (status, _) = call(&app, create_request(&ticket, &init_y)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!vault_exists(&fx, VAULT_Y));
    assert_eq!(block(|| ticket_row(&fx, &ticket)).quota, 1);

    // 坏证书（证书绑定 Y，请求是 X）→ 400，整体回滚（票据不消费、registry 仍 RESERVED）。
    let mut bad = init_request(VAULT_X, DEVICE_X, fx.manager_pub);
    bad.certificate = init_y.certificate.clone();
    let (status, body) = call(&app, create_request(&ticket, &bad)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&body), "INVALID_REQUEST");
    assert!(!vault_exists(&fx, VAULT_X));
    assert_eq!(
        registry_row(&fx, VAULT_X).expect("row").state,
        LifecycleState::Reserved,
        "失败 claim 不得把 registry 改成 ACTIVE"
    );
    assert_eq!(block(|| ticket_row(&fx, &ticket)).quota, 1);

    // 过期：时钟推进后 403，registry 保持 RESERVED（claim 失败不写库）。
    fx.clock.advance_secs(1_000);
    let (status, _) = call(&app, create_request(&ticket, &init_x)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        registry_row(&fx, VAULT_X).expect("row").state,
        LifecycleState::Reserved
    );
    assert_eq!(block(|| ticket_row(&fx, &ticket)).quota, 1);
    assert!(!vault_exists(&fx, VAULT_X));
    fx.clock.advance_secs(-1_000);

    // 取消：票据作废 → claim 403，不建库。
    assert_eq!(
        block(|| registry::cancel_reservation(fx.db.inner(), VAULT_X)).expect("cancel"),
        CancelOutcome::Cancelled {
            vault_id: VAULT_X,
            cancelled_at: BASE_TIME,
            invalidated_tickets: 1,
        }
    );
    let (status, _) = call(&app, create_request(&ticket, &init_x)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!vault_exists(&fx, VAULT_X));

    // 已存在的 vault：历史/测试夹具插入的新票据 → claim 409 ALREADY_INITIALIZED，且不消费。
    // （ADR 0018 S4b 起没有"对 ACTIVE vault 只发票据"的发行入口；这里直接种一行测试票据。）
    let existing_ticket = [0xE1u8; 32];
    block(|| {
        let ticket_hash = relay::ids::sha256(&existing_ticket);
        fx.db.inner().write_instance_blocking(move |conn| {
            conn.execute(
                "INSERT INTO create_tickets(token_hash, vault_id, created_at, expires_at, quota,
                                            consumed_at)
                 VALUES (?1, ?2, ?3, ?4, 1, NULL)",
                (
                    ticket_hash.as_slice(),
                    fx.vault_id.as_slice(),
                    BASE_TIME,
                    BASE_TIME + 900,
                ),
            )?;
            Ok(())
        })
    })
    .expect("seed ticket for active vault");
    let init_existing = init_request(fx.vault_id, DEVICE_X, fx.manager_pub);
    let (status, body) = call(&app, create_request(&existing_ticket, &init_existing)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error_code(&body), "ALREADY_INITIALIZED");
    let existing = block(|| ticket_row(&fx, &existing_ticket));
    assert_eq!(existing.quota, 1, "409 不得消费票据");
    assert_eq!(existing.consumed_at, None);
    assert_eq!(
        registry_row(&fx, fx.vault_id).expect("row").state,
        LifecycleState::Active
    );
    // 夹具 vault 仍是原样（没被第二个 genesis 覆盖）。
    assert_eq!(vault_count(&fx), 1);
}

// ---------------------------------------------------------------- 3) claim vs cancel / 双 claim

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claim_winning_then_cancel_conflicts_and_cancel_winning_blocks_claim() {
    // 顺序一：claim 先赢 → cancel 409，已建 vault 不被删除。
    let fx = block(|| fixture("registry-claim-then-cancel"));
    let router_claim_first = app(&fx);
    let ticket = block(|| reserve(&fx, VAULT_X));
    let init = init_request(VAULT_X, DEVICE_X, fx.manager_pub);
    let (status, _) = call(&router_claim_first, create_request(&ticket, &init)).await;
    assert_eq!(status, StatusCode::CREATED);
    let err = block(|| registry::cancel_reservation(fx.db.inner(), VAULT_X))
        .expect_err("claim 赢后 cancel 必须冲突");
    assert_eq!(err.code(), Some(relay::ErrorCode::CasConflict));
    assert_eq!(
        registry_row(&fx, VAULT_X).expect("row").state,
        LifecycleState::Active,
        "cancel 不得把 ACTIVE 改掉"
    );
    assert!(vault_exists(&fx, VAULT_X), "已创建 vault 不得被取消删除");

    // 顺序二：cancel 先赢 → claim 403，不建 vault。
    let fx = block(|| fixture("registry-cancel-then-claim"));
    let router_cancel_first = app(&fx);
    let ticket = block(|| reserve(&fx, VAULT_Y));
    assert!(matches!(
        block(|| registry::cancel_reservation(fx.db.inner(), VAULT_Y)).expect("cancel"),
        CancelOutcome::Cancelled { .. }
    ));
    let init = init_request(VAULT_Y, DEVICE_X, fx.manager_pub);
    let (status, _) = call(&router_cancel_first, create_request(&ticket, &init)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!vault_exists(&fx, VAULT_Y));
    assert_eq!(
        registry_row(&fx, VAULT_Y).expect("row").state,
        LifecycleState::Cancelled
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_claim_vs_cancel_has_exactly_one_winner() {
    let fx = block(|| fixture("registry-claim-cancel-race"));
    let app = app(&fx);
    let ticket = block(|| reserve(&fx, VAULT_Z));
    let init = init_request(VAULT_Z, DEVICE_X, fx.manager_pub);
    let db = fx.db.inner().clone();
    let claim = call(&app, create_request(&ticket, &init));
    let cancel = async move {
        tokio::task::spawn_blocking(move || registry::cancel_reservation(&db, VAULT_Z))
            .await
            .expect("join")
    };
    let (claim, cancel) = tokio::join!(claim, cancel);
    let state = registry_row(&fx, VAULT_Z).expect("row").state;
    match (claim.0, cancel) {
        (StatusCode::CREATED, Err(err)) => {
            assert_eq!(err.code(), Some(relay::ErrorCode::CasConflict));
            assert_eq!(state, LifecycleState::Active, "claim 赢 → ACTIVE");
            assert!(vault_exists(&fx, VAULT_Z));
            assert_eq!(vault_count(&fx), 2);
        }
        (StatusCode::FORBIDDEN, Ok(CancelOutcome::Cancelled { .. })) => {
            assert_eq!(state, LifecycleState::Cancelled, "cancel 赢 → CANCELLED");
            assert!(!vault_exists(&fx, VAULT_Z), "取消赢不得建 vault");
            assert_eq!(vault_count(&fx), 1);
        }
        other => panic!("claim/cancel 必须恰好一个赢，实际 {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_double_claim_has_exactly_one_winner() {
    let fx = block(|| fixture("registry-double-claim"));
    let app = app(&fx);
    let ticket = block(|| reserve(&fx, VAULT_X));
    let init = init_request(VAULT_X, DEVICE_X, fx.manager_pub);
    let (left, right) = tokio::join!(
        call(&app, create_request(&ticket, &init)),
        call(&app, create_request(&ticket, &init)),
    );
    let mut statuses = [left.0, right.0];
    statuses.sort_by_key(|status| status.as_u16());
    assert_eq!(
        statuses,
        [StatusCode::CREATED, StatusCode::FORBIDDEN],
        "并发双 claim 必须恰好一个成功"
    );
    assert_eq!(
        registry_row(&fx, VAULT_X).expect("row").state,
        LifecycleState::Active
    );
    assert_eq!(vault_count(&fx), 2, "只允许建出一个 vault");
}

// ---------------------------------------------------------------- 4) 列表/详情（只读）

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_and_detail_distinguish_states_without_writing() {
    let fx = block(|| fixture("registry-list"));
    let app = app(&fx);
    // RESERVED：有效票据。
    let reserved = block(|| {
        relay::admin::create_reservation_for(fx.db.inner(), VAULT_X, Duration::from_secs(900))
            .expect("reserve")
    });
    // 过期：TTL 60s，推进时钟 61s（读时推导 EXPIRED，不写库）。
    let expired = block(|| {
        relay::admin::create_reservation_for(fx.db.inner(), VAULT_Y, Duration::from_secs(60))
            .expect("reserve")
    });
    fx.clock.advance_secs(61);
    // ACTIVE：claim 成功。
    let active = block(|| {
        relay::admin::create_reservation_for(fx.db.inner(), VAULT_Z, Duration::from_secs(900))
            .expect("reserve")
    });
    let init = init_request(VAULT_Z, DEVICE_X, fx.manager_pub);
    let (status, _) = call(&app, create_request(&active.token, &init)).await;
    assert_eq!(status, StatusCode::CREATED);

    let now = fx.clock.now_secs();
    let registry_before = dump(&fx, "vault_registry");
    let tickets_before = dump(&fx, "create_tickets");
    let vaults_before = dump(&fx, "vaults");
    let entries = fx
        .db
        .read_instance(|conn| registry::list_vaults(conn, now))
        .expect("list");
    let by_id = |id: [u8; 16]| {
        entries
            .iter()
            .find(|entry| entry.vault_id == id)
            .cloned()
            .unwrap_or_else(|| panic!("missing {id:?}"))
    };
    // 夹具 legacy vault A 的 registry 由 genesis 同事务写成 ACTIVE。
    assert_eq!(by_id(fx.vault_id).state, LifecycleState::Active);
    assert!(by_id(fx.vault_id).vault_exists);
    let reserved_entry = by_id(VAULT_X);
    assert_eq!(reserved_entry.state, LifecycleState::Reserved);
    assert_eq!(reserved_entry.stored_state, LifecycleState::Reserved);
    assert_eq!(reserved_entry.expires_at, Some(reserved.expires_at));
    assert_eq!(reserved_entry.outstanding_tickets, 1);
    assert!(!reserved_entry.vault_exists);
    let expired_entry = by_id(VAULT_Y);
    assert_eq!(
        expired_entry.state,
        LifecycleState::Expired,
        "有效期已过的 RESERVED 在读时推导为 EXPIRED"
    );
    assert_eq!(
        expired_entry.stored_state,
        LifecycleState::Reserved,
        "推导不得写库：registry 里仍是 RESERVED"
    );
    assert!(!expired_entry.vault_exists);
    let active_entry = by_id(VAULT_Z);
    assert_eq!(active_entry.state, LifecycleState::Active);
    assert!(active_entry.vault_exists);
    assert_eq!(active_entry.outstanding_tickets, 0, "claim 后票据已用完");

    // 详情：ACTIVE 的租户事实从 `vaults` 读，RESERVED 没有租户内容。
    let detail = fx
        .db
        .read_instance(move |conn| registry::vault_detail(conn, &VAULT_Z, now))
        .expect("detail")
        .expect("active detail");
    assert_eq!(detail.entry.state, LifecycleState::Active);
    assert!(detail.entry.vault_exists);
    assert!(!detail.legacy_default);
    assert_eq!(detail.authz_state.as_deref(), Some("legacy"));
    assert_eq!(detail.active_epoch, Some(1));
    assert_eq!(detail.tickets_total, 1);
    assert_eq!(detail.tickets_consumed, 1);
    assert_eq!(detail.tickets_outstanding, 0);
    let reserved_detail = fx
        .db
        .read_instance(move |conn| registry::vault_detail(conn, &VAULT_X, now))
        .expect("detail")
        .expect("reserved detail");
    assert_eq!(reserved_detail.entry.state, LifecycleState::Reserved);
    assert!(reserved_detail.authz_state.is_none());
    assert!(reserved_detail.active_epoch.is_none());
    assert_eq!(reserved_detail.tickets_total, 1);
    assert_eq!(reserved_detail.tickets_consumed, 0);
    assert_eq!(reserved_detail.tickets_outstanding, 1);
    // 不存在的 id → None（existing-only）。
    assert!(fx
        .db
        .read_instance(|conn| registry::vault_detail(conn, &[0xEEu8; 16], now))
        .expect("detail")
        .is_none());

    // 只读：列表/详情不写 registry/票据/租户表。
    assert_eq!(dump(&fx, "vault_registry"), registry_before);
    assert_eq!(dump(&fx, "create_tickets"), tickets_before);
    assert_eq!(dump(&fx, "vaults"), vaults_before);

    // 显式取消过期预留 → registry 落 EXPIRED，票据作废（"已过期可标 EXPIRED"）。
    assert_eq!(
        block(|| registry::cancel_reservation(fx.db.inner(), VAULT_Y)).expect("cancel"),
        CancelOutcome::Expired
    );
    assert_eq!(
        registry_row(&fx, VAULT_Y).expect("row").state,
        LifecycleState::Expired
    );
    assert_eq!(
        block(|| ticket_row(&fx, &expired.token)).consumed_at,
        Some(now)
    );
    assert_eq!(
        block(|| registry::cancel_reservation(fx.db.inner(), VAULT_Y)).expect("idempotent"),
        CancelOutcome::Expired
    );
    // 取消有效预留 → CANCELLED + 票据作废；重复取消幂等。
    assert!(matches!(
        block(|| registry::cancel_reservation(fx.db.inner(), VAULT_X)).expect("cancel"),
        CancelOutcome::Cancelled { .. }
    ));
    assert_eq!(
        block(|| ticket_row(&fx, &reserved.token)).consumed_at,
        Some(now)
    );
    assert_eq!(
        block(|| registry::cancel_reservation(fx.db.inner(), VAULT_X)).expect("repeat"),
        CancelOutcome::AlreadyCancelled
    );
}
