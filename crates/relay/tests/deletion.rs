//! ADR 0018 **S3**：在线不可逆删除作业（fence / WS 关闭 / 分批 purge / 崩溃恢复）。
//!
//! 覆盖任务书 §5：
//! 1. `ACTIVE` 删除：`202` 语义（只在持久 `DELETING` 之后返回）、入口立刻 `410`、WS 关闭、
//!    终态 `DELETED`、25 张租户表 + 票据 + `vaults` 行零残留、其它 vault 不受影响；
//! 2. 双层 fence：admission（HTTP/WS）+ 事务级复核（scoped 读写、已入队但调用方取消的写）、
//!    GC 跳过、genesis/control/recovery/claim 全拒绝；
//! 3. 并发/幂等/失败重试/崩溃恢复（标记后、批次后、终态后重启）；
//! 4. 大 vault 分批 purge 期间其它租户读写不被长事务阻塞；行/WAL 预算有界；
//! 5. 受保护的 admin 删除触发路由（会话 + CSRF + confirm）。
//!
//! 全部真实 axum router + 临时 SQLite + `FakeClock`，**无 sleep**：收敛靠删除作业的
//! `broadcast` 事件（`Batch`/`Deleted`/`Failed`/`Stopped`）与 DB 状态，不靠时间。

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::Router;
use futures_util::StreamExt;
use http_body_util::BodyExt;
use tokio::sync::broadcast;
use tower::ServiceExt;

use relay::api::{router, run_gc, AppState};
use relay::auth::{self, Realm};
use relay::db::{Db, DbConfig};
use relay::deletion::{
    DeleteEvent, Deletion, PurgeFailpoint, PurgeLimits, StartOutcome, PURGE_ORDER,
};
use relay::ids::{hex_lower, Id16};
use relay::registry::{self, LifecycleState};
use relay::scope::VaultScope;
use relay::time::{Clock, FakeClock};
use relay::vault::{self, InitRequest};
use relay::ErrorCode;
use support::relay;
use support::{certificate_bytes, Fixture};

const BASE_TIME: i64 = 1_800_000_000;
/// 第二个租户（证明删除只影响目标 vault）。
const VAULT_B: Id16 = [0xB7u8; 16];
const DEVICE_B: Id16 = [0xB2u8; 16];
/// 删除作业的收敛等待（只是"必须收敛"的上界，不作为推进手段）。
const CONVERGE: Duration = Duration::from_secs(30);
const ADMIN_AUTHORITY: &str = "127.0.0.1:18081";
const ADMIN_ORIGIN: &str = "http://127.0.0.1:18081";

fn block<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}

// ---------------------------------------------------------------- 环境

struct Env {
    fx: Fixture,
    deletion: Arc<Deletion>,
    state: Arc<AppState>,
    app: Router,
}

impl Env {
    fn new(label: &str, limits: PurgeLimits) -> Self {
        let fx = block(|| Fixture::new(label));
        let deletion = Arc::new(Deletion::with_limits(fx.db.inner().clone(), limits));
        let state = AppState::with_deletion(
            fx.db.inner().clone(),
            fx.cfg.clone(),
            fx.hook.clone(),
            deletion.clone(),
        );
        let app = router(state.clone());
        Self {
            fx,
            deletion,
            state,
            app,
        }
    }

    fn vault(&self) -> Id16 {
        self.fx.vault_id
    }

    fn scope(&self) -> VaultScope {
        self.fx.db.scope()
    }
}

// ---------------------------------------------------------------- DB/HTTP 辅助

fn table_count(fx: &Fixture, table: &str, vault: Id16) -> i64 {
    fx.db
        .inner()
        .read_instance(move |conn| {
            Ok(conn.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE vault_id = ?1"),
                [vault.as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("count table")
}

fn ticket_count(fx: &Fixture, vault: Id16) -> i64 {
    fx.db
        .inner()
        .read_instance(move |conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM create_tickets WHERE vault_id = ?1",
                [vault.as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("count tickets")
}

fn registry_row(fx: &Fixture, vault: Id16) -> Option<registry::RegistryRow> {
    fx.db
        .inner()
        .read_instance(move |conn| registry::load_raw(conn, &vault))
        .expect("registry row")
}

fn registry_state(fx: &Fixture, vault: Id16) -> Option<LifecycleState> {
    registry_row(fx, vault).map(|row| row.state)
}

/// 26 张表（含 `vaults`）+ 票据表的残留计数（零残留断言）。
fn residue(fx: &Fixture, vault: Id16) -> Vec<(String, i64)> {
    let mut out: Vec<(String, i64)> = PURGE_ORDER
        .iter()
        .map(|table| ((*table).to_string(), table_count(fx, table, vault)))
        .collect();
    out.push(("create_tickets".to_string(), ticket_count(fx, vault)));
    out
}

fn assert_no_residue(fx: &Fixture, vault: Id16) {
    let leftovers: Vec<_> = residue(fx, vault)
        .into_iter()
        .filter(|(_, count)| *count != 0)
        .collect();
    assert!(
        leftovers.is_empty(),
        "删除后必须零残留，仍有行：{leftovers:?}"
    );
}

fn log_rows(fx: &Fixture, vault: Id16) -> i64 {
    table_count(fx, "log", vault)
}

/// 给 vault 造 `rows` 条合成 `log` 行（测试夹具；走 scoped 写 API）。
fn seed_log_rows(fx: &Fixture, scope: VaultScope, rows: usize, payload_len: usize) {
    let mut inserted = 0usize;
    while inserted < rows {
        let chunk = 64.min(rows - inserted);
        let first_seq = inserted as i64 + 1;
        let payload = vec![0x5Au8; payload_len];
        let clock = fx.clock.clone();
        fx.db
            .inner()
            .write_blocking(scope, move |conn| {
                let mut stmt = conn.prepare(
                    "INSERT INTO log(vault_id, relay_seq, kind, object_id, epoch, event,
                                     signature, created_at)
                     VALUES (?1, ?2, 'update', NULL, 1, ?3, NULL, ?4)",
                )?;
                for offset in 0..chunk {
                    stmt.execute((
                        scope.as_slice(),
                        first_seq + offset as i64,
                        payload.as_slice(),
                        clock.now_secs(),
                    ))?;
                }
                Ok(())
            })
            .expect("seed log rows");
        inserted += chunk;
    }
}

/// 造一张创建票据（`consumed` = 历史已消费/已作废行）。
fn seed_ticket(fx: &Fixture, vault: Id16, token_hash: [u8; 32], consumed: bool) {
    let now = fx.clock.now_secs();
    fx.db
        .inner()
        .write_instance_blocking(move |conn| {
            conn.execute(
                "INSERT INTO create_tickets(token_hash, vault_id, created_at, expires_at,
                                            quota, consumed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                (
                    token_hash.as_slice(),
                    vault.as_slice(),
                    now,
                    now + 900,
                    i64::from(!consumed),
                    consumed.then_some(now),
                ),
            )?;
            Ok(())
        })
        .expect("seed ticket");
}

/// 给 vault A 造**每一张租户表**至少一行（FK 依赖顺序），证明 purge 拓扑正确。
fn seed_dense_vault(fx: &Fixture, scope: VaultScope, device: Id16) {
    let now = fx.clock.now_secs();
    fx.db
        .inner()
        .write_blocking(scope, move |conn| {
            let vault = scope.as_slice();
            // 维护 reservation（objects.reservation_id 指向它）。
            conn.execute(
                "INSERT INTO maintenance_reservations(vault_id, reservation_id, purpose, resource_id,
                     owner_device, reserved_bytes, consumed_bytes, state, expires_at)
                 VALUES (?1, ?2, 'checkpoint', ?3, ?4, 0, 0, 'active', ?5)",
                (
                    vault,
                    [0x72u8; 16].as_slice(),
                    [0x73u8; 16].as_slice(),
                    device.as_slice(),
                    now + 3600,
                ),
            )?;
            // 数据对象 + 分片。
            conn.execute(
                "INSERT INTO objects(vault_id, object_id, message_id, producer, epoch, type,
                     generation, sender_seq, part_count, plain_len, state, budget_class, digest,
                     reservation_id, created_at, touched_at)
                 VALUES (?1, ?2, ?3, ?4, 1, 1, 0, 0, 1, 300, 'complete', 'maintenance', ?5, ?6, ?7, ?7)",
                (
                    vault,
                    [0x71u8; 32].as_slice(),
                    [0x61u8; 16].as_slice(),
                    [0x62u8; 16].as_slice(),
                    [0x74u8; 32].as_slice(),
                    [0x72u8; 16].as_slice(),
                    now,
                ),
            )?;
            conn.execute(
                "INSERT INTO object_parts(vault_id, object_id, part_no, envelope, digest)
                 VALUES (?1, ?2, 0, ?3, ?4)",
                (
                    vault,
                    [0x71u8; 32].as_slice(),
                    vec![0xAAu8; 300],
                    [0x74u8; 32].as_slice(),
                ),
            )?;
            // key slot 对象 + 槽（slot_id FK → objects.object_id）。
            conn.execute(
                "INSERT INTO objects(vault_id, object_id, message_id, producer, epoch, type,
                     generation, sender_seq, part_count, plain_len, state, budget_class, digest,
                     created_at, touched_at)
                 VALUES (?1, ?2, ?3, ?4, 1, 5, 0, 1, 1, 1, 'complete', 'normal', ?5, ?6, ?6)",
                (
                    vault,
                    [0x91u8; 32].as_slice(),
                    [0x92u8; 16].as_slice(),
                    [0x93u8; 16].as_slice(),
                    [0x94u8; 32].as_slice(),
                    now,
                ),
            )?;
            conn.execute(
                "INSERT INTO key_slots(vault_id, slot_id, epoch, kind, descriptor, state,
                     slot_set_rev, created_at)
                 VALUES (?1, ?2, 1, 'password', ?3, 'staging', 0, ?4)",
                (
                    vault,
                    [0x91u8; 32].as_slice(),
                    b"slot-descriptor".to_vec(),
                    now,
                ),
            )?;
            // checkpoint + refs。
            conn.execute(
                "INSERT INTO checkpoints(vault_id, checkpoint_id, epoch, covered_seq, state,
                     validation_version, health, created_at)
                 VALUES (?1, ?2, 1, 0, 'staging', 1, 'unknown', ?3)",
                (vault, [0x75u8; 16].as_slice(), now),
            )?;
            conn.execute(
                "INSERT INTO checkpoint_refs(vault_id, checkpoint_id, object_id, role)
                 VALUES (?1, ?2, ?3, 'data')",
                (
                    vault,
                    [0x75u8; 16].as_slice(),
                    [0x71u8; 32].as_slice(),
                ),
            )?;
            // asset 登记。
            conn.execute(
                "INSERT INTO asset_objects(vault_id, epoch, object_id, state, finalized_seq)
                 VALUES (?1, 1, ?2, 'active', 0)",
                (vault, [0x76u8; 32].as_slice()),
            )?;
            // 鉴权临时行。
            conn.execute(
                "INSERT INTO challenges(vault_id, challenge_id, realm, subject, nonce, expires_at)
                 VALUES (?1, ?2, 1, ?3, ?4, ?5)",
                (
                    vault,
                    [0x77u8; 16].as_slice(),
                    device.as_slice(),
                    [0x78u8; 32].as_slice(),
                    now + 60,
                ),
            )?;
            conn.execute(
                "INSERT INTO tokens(vault_id, token_hash, realm, subject, auth_rev, expires_at,
                     created_at)
                 VALUES (?1, ?2, 1, ?3, 1, ?4, ?5)",
                (
                    vault,
                    [0x79u8; 32].as_slice(),
                    device.as_slice(),
                    now + 600,
                    now,
                ),
            )?;
            conn.execute(
                "INSERT INTO invites(vault_id, invite_id, token_hash, expires_at)
                 VALUES (?1, ?2, ?3, ?4)",
                (
                    vault,
                    [0x7Au8; 16].as_slice(),
                    [0x7Bu8; 32].as_slice(),
                    now + 900,
                ),
            )?;
            // 管理 cap（FK：devices + control_commits(rev=1)）。
            conn.execute(
                "INSERT INTO management_caps(vault_id, cap_id, canonical_bytes, subject_device,
                     subject_pk, scopes, not_before, expires_at, issued_control_rev,
                     issued_authz_rev, issuer_authority_rev, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, 1, 1, 1, ?6)",
                (
                    vault,
                    [0x7Cu8; 16].as_slice(),
                    b"canonical-cap".to_vec(),
                    device.as_slice(),
                    [0x7Du8; 32].as_slice(),
                    now,
                    now + 900,
                ),
            )?;
            // 设备凭证历史 / grant / lease / ack（FK → devices）。
            conn.execute(
                "INSERT INTO device_credentials(vault_id, device_id, serial, authz_version,
                     sign_pk, hpke_pk, certificate, issued_authz_rev, approval_control_rev,
                     created_at)
                 VALUES (?1, ?2, 1, 1, ?3, ?4, ?5, 0, 1, ?6)",
                (
                    vault,
                    device.as_slice(),
                    [0x81u8; 32].as_slice(),
                    [0x82u8; 32].as_slice(),
                    b"credential".to_vec(),
                    now,
                ),
            )?;
            conn.execute(
                "INSERT INTO device_grants(vault_id, epoch, device_id, manager_rev, hpke_enc,
                     hpke_ciphertext, signed_header, approval_control_rev)
                 VALUES (?1, 1, ?2, 1, ?3, ?4, ?5, 1)",
                (
                    vault,
                    device.as_slice(),
                    [0x83u8; 32].as_slice(),
                    b"grant-ciphertext".to_vec(),
                    b"grant-header".to_vec(),
                ),
            )?;
            conn.execute(
                "INSERT INTO bootstrap_leases(vault_id, lease_id, device_id, primary_checkpoint,
                     fallback_checkpoint, floor_seq, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?4, 0, ?5, ?6)",
                (
                    vault,
                    [0x84u8; 16].as_slice(),
                    device.as_slice(),
                    [0x75u8; 16].as_slice(),
                    now,
                    now + 3600,
                ),
            )?;
            conn.execute(
                "INSERT INTO device_acks(vault_id, device_id, received_seq, integrated_seq, epoch,
                     updated_at)
                 VALUES (?1, ?2, 0, 0, 1, ?3)",
                (vault, device.as_slice(), now),
            )?;
            // 轮换 / 幂等 / GC 计划。
            conn.execute(
                "INSERT INTO rotations(vault_id, rotation_id, from_epoch, to_epoch, freeze_seq,
                     revoked_count, state, created_at, updated_at)
                 VALUES (?1, ?2, 1, 2, 0, 0, 'frozen', ?3, ?3)",
                (vault, [0x85u8; 16].as_slice(), now),
            )?;
            conn.execute(
                "INSERT INTO dedup(vault_id, scope, id, request_hash, accepted_seq, response,
                     created_at)
                 VALUES (?1, 'management/test', ?2, ?3, NULL, ?4, ?5)",
                (
                    vault,
                    [0x86u8; 16].as_slice(),
                    [0x87u8; 32].as_slice(),
                    b"dedup-response".to_vec(),
                    now,
                ),
            )?;
            conn.execute(
                "INSERT INTO dedup_v2(vault_id, request_id, request_hash, control_rev,
                     accepted_seq, response, created_at)
                 VALUES (?1, ?2, ?3, 1, NULL, ?4, ?5)",
                (
                    vault,
                    [0x88u8; 16].as_slice(),
                    [0x89u8; 32].as_slice(),
                    b"dedup-v2-response".to_vec(),
                    now,
                ),
            )?;
            conn.execute(
                "INSERT INTO gc_plans(vault_id, epoch, cut_seq, eligible_at, proof, created_at)
                 VALUES (?1, 1, 0, ?2, ?3, ?2)",
                (
                    vault,
                    now,
                    b"gc-proof".to_vec(),
                ),
            )?;
            Ok(())
        })
        .expect("seed dense vault");
    seed_log_rows(fx, scope, 3, 128);
    seed_ticket(fx, *scope.vault_id(), [0xA1u8; 32], false);
    seed_ticket(fx, *scope.vault_id(), [0xA2u8; 32], true);
}

fn create_second_vault(fx: &Fixture) -> Id16 {
    let clock = fx.clock.clone();
    let hook = fx.hook.clone();
    let cfg = fx.cfg.clone();
    let sign_pk = fx.device_signing_key.verifying_key().to_bytes();
    let hpke_pk = [4u8; 32];
    let init = InitRequest {
        vault_id: VAULT_B,
        device_id: DEVICE_B,
        sign_pk,
        hpke_pk,
        manager_pub: fx.manager_pub,
        setup_secret: "setup-secret".to_string(),
        certificate: certificate_bytes(&VAULT_B, &DEVICE_B, &sign_pk, &hpke_pk, 1),
        certificate_signature: b"certificate-sig".to_vec(),
    };
    fx.db
        .inner()
        .write_blocking(VaultScope::new(VAULT_B), move |conn| {
            vault::create_vault_in_scope(conn, clock.as_ref(), &cfg, hook.as_ref(), &init, false)
        })
        .expect("create second vault");
    VAULT_B
}

fn device_token_for(fx: &Fixture, scope: VaultScope, device: Id16) -> String {
    let clock = fx.clock.clone();
    let challenge = fx
        .db
        .inner()
        .write_blocking(scope, {
            let clock = clock.clone();
            move |conn| auth::create_challenge(conn, clock.as_ref(), Realm::Device, &device)
        })
        .expect("challenge");
    let hook = fx.hook.clone();
    fx.db
        .inner()
        .write_blocking(scope, move |conn| {
            auth::issue_token(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &challenge.challenge_id,
                b"challenge-message",
                b"signature",
            )
        })
        .expect("token")
        .token
}

fn genesis_body(req: &InitRequest) -> Vec<u8> {
    relay::wire::encode(vec![
        (0, relay::wire::bytes(req.vault_id)),
        (1, relay::wire::bytes(req.device_id)),
        (2, relay::wire::bytes(req.sign_pk)),
        (3, relay::wire::bytes(req.hpke_pk)),
        (4, relay::wire::bytes(req.manager_pub)),
        (5, relay::wire::text(req.setup_secret.clone())),
        (6, relay::wire::bytes(req.certificate.clone())),
        (7, relay::wire::bytes(req.certificate_signature.clone())),
    ])
    .expect("encode genesis body")
}

struct Reply {
    status: StatusCode,
    body: Vec<u8>,
}

impl Reply {
    fn error_code(&self) -> String {
        let fields = relay::wire::decode(&self.body).expect("CDE error body");
        relay::wire::text_field(&fields, 0).expect("error code")
    }

    fn error_retryable(&self) -> bool {
        let fields = relay::wire::decode(&self.body).expect("CDE error body");
        relay::wire::bool_field(&fields, 1).expect("error retryable")
    }
}

async fn call(app: &Router, request: Request<Body>) -> Reply {
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    Reply { status, body }
}

fn state_request(prefix: &str, vault: Id16, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("{prefix}/vaults/{}/state", hex_lower(&vault)))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("state request")
}

async fn start_delete(deletion: &Arc<Deletion>, vault: Id16) -> StartOutcome {
    deletion.start(vault).await.expect("start delete")
}

/// 等该 vault 的终态/停止事件（`Deleted`/`Failed`/`Stopped`）；**不用 sleep**。
async fn drain_until_terminal(
    rx: &mut broadcast::Receiver<DeleteEvent>,
    vault: Id16,
) -> (Vec<DeleteEvent>, DeleteEvent) {
    tokio::time::timeout(CONVERGE, async {
        let mut seen = Vec::new();
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let terminal = match &event {
                        DeleteEvent::Deleted { vault_id, .. }
                        | DeleteEvent::Failed { vault_id, .. }
                        | DeleteEvent::Stopped { vault_id, .. } => *vault_id == vault,
                        _ => false,
                    };
                    seen.push(event.clone());
                    if terminal {
                        return (seen, event);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    panic!("删除事件订阅落后 {skipped} 条（测试阈值应 ≥ 批次数）");
                }
                Err(err) => panic!("删除事件通道关闭：{err}"),
            }
        }
    })
    .await
    .expect("等待删除作业终态超时")
}

fn small_limits() -> PurgeLimits {
    PurgeLimits {
        rows_per_batch: 4,
        chunk_rows: 2,
        bytes_per_batch: 1024 * 1024,
        millis_per_batch: 25,
        wal_budget_bytes: 1024 * 1024,
    }
}

// ---------------------------------------------------------------- 1) 完整删除

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_delete_is_persistent_closes_admission_and_purges_zero_residue() {
    let env = Env::new("del-complete", small_limits());
    let vault = env.vault();
    let scope = env.scope();
    block(|| {
        seed_dense_vault(&env.fx, scope, env.fx.device);
        create_second_vault(&env.fx);
        seed_log_rows(&env.fx, VaultScope::new(VAULT_B), 5, 64);
    });
    assert_eq!(log_rows(&env.fx, vault), 3);
    let token_a = block(|| device_token_for(&env.fx, scope, env.fx.device));
    let token_b = block(|| device_token_for(&env.fx, VaultScope::new(VAULT_B), DEVICE_B));
    let health_b = call(&env.app, state_request("/v2", VAULT_B, &token_b)).await;
    assert_eq!(health_b.status, StatusCode::OK, "B 删除前应正常");

    // 先停在第 0 批之前：这样"标记已持久 / purge 未动"的断言与后台任务无关（无 sleep 竞态）。
    env.deletion
        .set_failpoint(Some(PurgeFailpoint::StopAfterBatches(0)));
    let mut events = env.deletion.subscribe();
    let outcome = start_delete(&env.deletion, vault).await;
    assert!(outcome.started(), "首次删除必须是新作业：{outcome:?}");
    let job_id = outcome.job().job_id.expect("job id");

    // `202` 语义：返回时 registry 已经是持久 `DELETING`，票据/临时认证已作废。
    let row = registry_row(&env.fx, vault).expect("registry row");
    assert_eq!(row.state, LifecycleState::Deleting);
    assert_eq!(row.delete_job_id, Some(job_id));
    assert_eq!(row.delete_progress, 0);
    assert_eq!(
        row.deleted_at,
        Some(BASE_TIME),
        "deleted_at 进行中 = 删除请求时刻"
    );
    assert_eq!(
        ticket_count(&env.fx, vault),
        2,
        "票据行留给 purge，但已作废"
    );
    let outstanding: i64 = env
        .fx
        .db
        .inner()
        .read_instance(move |conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM create_tickets WHERE vault_id = ?1 AND consumed_at IS NULL",
                [vault.as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("outstanding tickets");
    assert_eq!(outstanding, 0, "标记事务必须作废全部未消费票据");
    assert_eq!(table_count(&env.fx, "tokens", vault), 0);
    assert_eq!(table_count(&env.fx, "challenges", vault), 0);
    let (_, stopped) = drain_until_terminal(&mut events, vault).await;
    assert!(
        matches!(stopped, DeleteEvent::Stopped { .. }),
        "{stopped:?}"
    );

    // 入口立刻关闭（不需要等 purge）。
    for (prefix, token) in [("/v1", token_a.as_str()), ("/v2", token_a.as_str())] {
        let reply = call(&env.app, state_request(prefix, vault, token)).await;
        assert_eq!(reply.status, StatusCode::GONE, "{prefix} 删除态必须 410");
        assert_eq!(reply.error_code(), "VAULT_DELETED");
        assert!(!reply.error_retryable());
    }
    // control / recovery 同样关闭（middleware 在解析/鉴权之前拒绝）。
    let control = Request::builder()
        .method("POST")
        .uri(format!("/v2/vaults/{}/control/submit", hex_lower(&vault)))
        .header(header::AUTHORIZATION, format!("Bearer {token_a}"))
        .header(header::CONTENT_TYPE, relay::wire::CONTENT_TYPE)
        .body(Body::empty())
        .expect("control request");
    let reply = call(&env.app, control).await;
    assert_eq!(reply.status, StatusCode::GONE);
    assert_eq!(reply.error_code(), "VAULT_DELETED");
    let recovery = Request::builder()
        .method("GET")
        .uri(format!(
            "/v2/vaults/{}/recovery/evidence",
            hex_lower(&vault)
        ))
        .header(header::AUTHORIZATION, format!("Bearer {token_a}"))
        .body(Body::empty())
        .expect("recovery request");
    assert_eq!(call(&env.app, recovery).await.status, StatusCode::GONE);

    // 解除 failpoint，重新武装同一 job（`AlreadyDeleting`）并收敛到终态。
    env.deletion.shutdown().await;
    env.deletion.set_failpoint(None);
    let resumed = start_delete(&env.deletion, vault).await;
    assert!(matches!(resumed, StartOutcome::AlreadyDeleting(_)));
    let (events, terminal) = drain_until_terminal(&mut events, vault).await;
    assert!(
        matches!(terminal, DeleteEvent::Deleted { .. }),
        "{terminal:?}"
    );
    assert!(events
        .iter()
        .any(|event| matches!(event, DeleteEvent::Batch { .. })));
    let row = registry_row(&env.fx, vault).expect("tombstone row");
    assert_eq!(row.state, LifecycleState::Deleted);
    assert_eq!(row.delete_job_id, Some(job_id), "墓碑保留作业 id");
    assert_eq!(row.delete_progress, PURGE_ORDER.len() as u64);
    assert_eq!(row.delete_error, None);
    assert!(
        row.deleted_at.expect("完成时刻") >= BASE_TIME,
        "DELETED 的 deleted_at = 完成时刻"
    );
    assert!(row.deleted_at.is_some());
    assert_no_residue(&env.fx, vault);

    // 其它 vault 完全不受影响：registry/数据面/票据都在。
    assert_eq!(
        registry_state(&env.fx, VAULT_B),
        Some(LifecycleState::Active)
    );
    assert_eq!(log_rows(&env.fx, VAULT_B), 5);
    let health_b = call(&env.app, state_request("/v2", VAULT_B, &token_b)).await;
    assert_eq!(health_b.status, StatusCode::OK, "其它 vault 必须继续可服务");

    // 删除态/墓碑永不重开：再走 v1 genesis 只会被 fence 拒绝，registry 不变。
    let request = block(|| InitRequest {
        vault_id: vault,
        device_id: env.fx.device,
        sign_pk: env.fx.device_signing_key.verifying_key().to_bytes(),
        hpke_pk: [4u8; 32],
        manager_pub: env.fx.manager_pub,
        setup_secret: "setup-secret".to_string(),
        certificate: certificate_bytes(
            &vault,
            &env.fx.device,
            &env.fx.device_signing_key.verifying_key().to_bytes(),
            &[4u8; 32],
            1,
        ),
        certificate_signature: b"certificate-sig".to_vec(),
    });
    let body = genesis_body(&request);
    let genesis = Request::builder()
        .method("POST")
        .uri("/v1/vaults")
        .header(header::CONTENT_TYPE, relay::wire::CONTENT_TYPE)
        .body(Body::from(body))
        .expect("genesis request");
    let reply = call(&env.app, genesis).await;
    assert_eq!(reply.status, StatusCode::GONE, "墓碑不得重开 ACTIVE");
    assert_eq!(
        registry_state(&env.fx, vault),
        Some(LifecycleState::Deleted),
        "失败的重开不得改 registry"
    );
    // 重复删除：幂等返回同一 job。
    let again = start_delete(&env.deletion, vault).await;
    assert!(matches!(again, StartOutcome::AlreadyDeleted(_)));
    assert_eq!(again.job().job_id, Some(job_id));
}

// ---------------------------------------------------------------- 2) 双层 fence

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_layer_fence_blocks_queued_writes_cancelled_http_and_gc() {
    let env = Env::new("del-fence", small_limits());
    let vault = env.vault();
    let scope = env.scope();
    block(|| {
        seed_log_rows(&env.fx, scope, 12, 128);
        create_second_vault(&env.fx);
        seed_log_rows(&env.fx, VaultScope::new(VAULT_B), 4, 64);
    });
    // 停在第 0 批之前：持久 DELETING 但 purge 一行没删（方便断言 fence 本身）。
    env.deletion
        .set_failpoint(Some(PurgeFailpoint::StopAfterBatches(0)));
    let mut events = env.deletion.subscribe();
    let outcome = start_delete(&env.deletion, vault).await;
    assert!(outcome.started());
    let (_, terminal) = drain_until_terminal(&mut events, vault).await;
    assert!(
        matches!(terminal, DeleteEvent::Stopped { .. }),
        "{terminal:?}"
    );
    assert_eq!(
        registry_state(&env.fx, vault),
        Some(LifecycleState::Deleting)
    );
    assert_eq!(log_rows(&env.fx, vault), 12, "purge 未跑，行仍在");

    // (a) 事务级写复核：排在标记之后的 scoped 写被拒绝，且不落库。
    let db = env.fx.db.inner().clone();
    let err = db
        .write(scope, move |conn| {
            conn.execute(
                "INSERT INTO log(vault_id, relay_seq, kind, epoch, event, created_at)
                 VALUES (?1, 999, 'update', 1, X'00', 0)",
                [scope.as_slice()],
            )?;
            Ok(())
        })
        .await
        .expect_err("删除态写必须被事务级 fence 拒绝");
    assert_eq!(err.code(), Some(ErrorCode::VaultDeleted));
    assert_eq!(log_rows(&env.fx, vault), 12, "被拒绝的写不得落库");

    // (b) "HTTP 已取消"不等于不执行：已入队 job 的 future 被丢弃后仍会跑，但事务级复核
    //     让它 commit 不了（单 writer FIFO + 事务开始时的权威复核 = 显式屏障）。
    let db = env.fx.db.inner().clone();
    let mut cancelled = Box::pin(db.write(scope, move |conn| {
        conn.execute(
            "INSERT INTO log(vault_id, relay_seq, kind, epoch, event, created_at)
             VALUES (?1, 1000, 'update', 1, X'00', 0)",
            [scope.as_slice()],
        )?;
        Ok(())
    }));
    // biased 先轮询写 future（send 已就绪 → job 已入队），再被 yield 让出；此时丢弃 future
    // 只是"HTTP 连接断了"，job 仍会执行，并在事务开始时被 fence 拒绝。
    let early = tokio::select! {
        biased;
        done = &mut cancelled => Some(done),
        _ = tokio::task::yield_now() => None,
    };
    drop(cancelled);
    if let Some(done) = early {
        assert!(done.is_err(), "取消前的写不可能成功：{done:?}");
    }
    // 屏障：writer 队列先跑完前面已入队的 job（不是 sleep）。
    db.write_maintenance(|_| Ok(()))
        .await
        .expect("writer barrier");
    assert_eq!(
        log_rows(&env.fx, vault),
        12,
        "取消的 HTTP 请求不得绕过 fence"
    );

    // (c) 读路径复核：删除态不返回任何旧快照数据。
    let err = db
        .read(scope, |conn| {
            Ok(conn.query_row("SELECT COUNT(*) FROM log", [], |row| row.get::<_, i64>(0))?)
        })
        .expect_err("删除态读必须被拒绝");
    assert_eq!(err.code(), Some(ErrorCode::VaultDeleted));

    // (d) GC 跳过 fenced vault（不得推进/删除它的行），其它 vault 照常跑。
    run_gc(env.state.clone()).await.expect("gc");
    assert_eq!(log_rows(&env.fx, vault), 12, "GC 不得清理被 fence 的 vault");
    assert_eq!(
        registry_state(&env.fx, vault),
        Some(LifecycleState::Deleting)
    );

    // (e) 匿名 genesis 走的是 `/v1/vaults`（没有路径 vault → middleware 不拦，
    //     必须由事务级复核拒绝），证明双层 fence 两层都真的在。
    let request = block(|| InitRequest {
        vault_id: vault,
        device_id: env.fx.device,
        sign_pk: env.fx.device_signing_key.verifying_key().to_bytes(),
        hpke_pk: [4u8; 32],
        manager_pub: env.fx.manager_pub,
        setup_secret: "setup-secret".to_string(),
        certificate: certificate_bytes(
            &vault,
            &env.fx.device,
            &env.fx.device_signing_key.verifying_key().to_bytes(),
            &[4u8; 32],
            1,
        ),
        certificate_signature: b"certificate-sig".to_vec(),
    });
    let reply = call(
        &env.app,
        Request::builder()
            .method("POST")
            .uri("/v1/vaults")
            .header(header::CONTENT_TYPE, relay::wire::CONTENT_TYPE)
            .body(Body::from(genesis_body(&request)))
            .expect("genesis request"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::GONE);
    assert_eq!(reply.error_code(), "VAULT_DELETED");

    // 收尾：解除 failpoint 并让作业真正跑完（证明它还能继续）。
    env.deletion.shutdown().await;
    env.deletion.set_failpoint(None);
    let retried = start_delete(&env.deletion, vault).await;
    assert!(matches!(retried, StartOutcome::AlreadyDeleting(_)));
    let (_, terminal) = drain_until_terminal(&mut events, vault).await;
    assert!(
        matches!(terminal, DeleteEvent::Deleted { .. }),
        "{terminal:?}"
    );
    assert_no_residue(&env.fx, vault);
    assert_eq!(log_rows(&env.fx, VAULT_B), 4);
}

// ---------------------------------------------------------------- 3) WS

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_closes_wake_socket_and_rejects_new_upgrades() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message;

    let env = Env::new("del-ws", small_limits());
    let vault = env.vault();
    let token = block(|| env.fx.device_token());
    let app = router(env.state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let uri = format!("ws://{addr}/v2/vaults/{}/wake", hex_lower(&vault));
    let mut request = uri.clone().into_client_request().expect("ws request");
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {token}").parse().expect("header"),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("ws connect");
    // 首帧：{v:1, head_seq, epoch, control_rev, ...}
    let first = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("initial frame timeout")
        .expect("initial frame")
        .expect("initial frame ok");
    assert!(matches!(first, Message::Text(_)));

    let mut events = env.deletion.subscribe();
    let outcome = start_delete(&env.deletion, vault).await;
    assert!(outcome.started());

    // 删除开始 → 服务端主动 Close(1008)，不是等客户端超时。
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await {
                None => return Some(None),
                Some(Ok(Message::Close(frame))) => return Some(frame),
                Some(Err(_)) => return Some(None),
                Some(Ok(_)) => continue,
            }
        }
    })
    .await
    .expect("ws close timeout")
    .expect("close frame or EOF");
    if let Some(frame) = closed {
        assert_eq!(
            u16::from(frame.code),
            1008,
            "删除必须用 1008 policy violation"
        );
    }

    // 之后的新升级一律被 admission 拒绝（连接建立失败）。
    let mut again = uri.into_client_request().expect("ws request");
    again.headers_mut().insert(
        "Authorization",
        format!("Bearer {token}").parse().expect("header"),
    );
    assert!(
        tokio_tungstenite::connect_async(again).await.is_err(),
        "删除态不得再建立 WS"
    );

    let (_, terminal) = drain_until_terminal(&mut events, vault).await;
    assert!(matches!(terminal, DeleteEvent::Deleted { .. }));
}

// ---------------------------------------------------------------- 4) 并发/幂等

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_deletes_dedup_to_one_job_and_writes_never_land_after_fence() {
    let env = Env::new("del-concurrent", small_limits());
    let vault = env.vault();
    let scope = env.scope();
    block(|| seed_log_rows(&env.fx, scope, 24, 256));
    let mut events = env.deletion.subscribe();

    let first = env.deletion.start(vault);
    let second = env.deletion.start(vault);
    let (first, second) = tokio::join!(first, second);
    let first = first.expect("first delete");
    let second = second.expect("second delete");
    let job_id = first.job().job_id.expect("job id");
    assert_eq!(
        second.job().job_id,
        Some(job_id),
        "重复请求必须返回同一 job"
    );
    let started_count = u8::from(matches!(first, StartOutcome::Started(_)))
        + u8::from(matches!(second, StartOutcome::Started(_)));
    assert_eq!(started_count, 1, "并发重复删除只能有一个 Started");

    // 删除进行中并发写：要么被 fence 拒绝（VaultDeleted），要么在标记前已提交；都不得破坏终态。
    let db = env.fx.db.inner().clone();
    let mut handles = Vec::new();
    for seq in 100..108 {
        let db = db.clone();
        handles.push(tokio::spawn(async move {
            db.write(scope, move |conn| {
                conn.execute(
                    "INSERT INTO log(vault_id, relay_seq, kind, epoch, event, created_at)
                     VALUES (?1, ?2, 'update', 1, X'00', 0)",
                    (scope.as_slice(), seq),
                )?;
                Ok(())
            })
            .await
        }));
    }
    for handle in handles {
        // 作业可能已经围栏（VaultDeleted）或没抢到标记前的窗口（Ok）；其它错误都是 bug。
        match handle.await.expect("join") {
            Ok(()) => {}
            Err(err) => assert_eq!(err.code(), Some(ErrorCode::VaultDeleted)),
        }
    }

    let (_, terminal) = drain_until_terminal(&mut events, vault).await;
    assert!(
        matches!(terminal, DeleteEvent::Deleted { .. }),
        "{terminal:?}"
    );
    assert_no_residue(&env.fx, vault);
    assert_eq!(
        registry_state(&env.fx, vault),
        Some(LifecycleState::Deleted)
    );
    let again = start_delete(&env.deletion, vault).await;
    assert!(matches!(again, StartOutcome::AlreadyDeleted(_)));
    assert_eq!(again.job().job_id, Some(job_id));
}

// ---------------------------------------------------------------- 5) 失败重试

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_failure_keeps_same_job_and_retry_completes() {
    let env = Env::new("del-retry", small_limits());
    let vault = env.vault();
    block(|| seed_log_rows(&env.fx, env.scope(), 12, 128));
    // 删除前签 token（删除态连 challenge 都会拒），用于失败态的数据面断言。
    let token = block(|| env.fx.device_token());
    env.deletion
        .set_failpoint(Some(PurgeFailpoint::FailAfterBatches(1)));
    let mut events = env.deletion.subscribe();
    let outcome = start_delete(&env.deletion, vault).await;
    let job_id = outcome.job().job_id.expect("job id");

    let (_, terminal) = drain_until_terminal(&mut events, vault).await;
    assert!(
        matches!(terminal, DeleteEvent::Failed { .. }),
        "{terminal:?}"
    );
    let row = registry_row(&env.fx, vault).expect("row");
    assert_eq!(row.state, LifecycleState::DeleteFailed);
    assert_eq!(row.delete_job_id, Some(job_id), "失败保留同一 job");
    assert!(row.delete_progress > 0, "失败保留进度");
    assert_eq!(row.deleted_at, Some(BASE_TIME), "失败不改删除请求时刻");
    assert!(
        row.delete_error
            .as_deref()
            .is_some_and(|err| err.contains("injected")),
        "失败原因必须是短错误码：{:?}",
        row.delete_error
    );

    // 失败态同样关闭数据面（admission 不因失败而打开）。
    let reply = call(&env.app, state_request("/v2", vault, &token)).await;
    assert_eq!(reply.status, StatusCode::GONE);
    assert_eq!(reply.error_code(), "VAULT_DELETED");

    // 同一 job 重试（先等作业任务真正退出，再清 failpoint 收敛到 DELETED）。
    env.deletion.shutdown().await;
    env.deletion.set_failpoint(None);
    let retried = start_delete(&env.deletion, vault).await;
    assert!(matches!(retried, StartOutcome::Retried(_)), "{retried:?}");
    assert_eq!(retried.job().job_id, Some(job_id), "重试必须复用同一 job");
    let (_, terminal) = drain_until_terminal(&mut events, vault).await;
    assert!(
        matches!(terminal, DeleteEvent::Deleted { .. }),
        "{terminal:?}"
    );
    let row = registry_row(&env.fx, vault).expect("row");
    assert_eq!(row.state, LifecycleState::Deleted);
    assert_eq!(row.delete_error, None);
    assert_no_residue(&env.fx, vault);
}

// ---------------------------------------------------------------- 6) 崩溃恢复

/// 关闭句柄并重开同一个库（模拟进程重启；`FakeClock` 保持同一时刻）。
fn restart_db(fx: Fixture) -> (PathBuf, Db) {
    let path = fx.cfg.db.path.clone();
    let kept = fx.into_db_path();
    assert_eq!(path, kept);
    let db = Db::open(
        DbConfig::new(path.clone()),
        Arc::new(FakeClock::new(BASE_TIME)),
    )
    .expect("reopen db");
    (path, db)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_after_marking_resumes_deleting_job_before_serving() {
    let env = Env::new("del-recover-mark", small_limits());
    let vault = env.vault();
    block(|| {
        seed_log_rows(&env.fx, env.scope(), 12, 128);
        seed_ticket(&env.fx, vault, [0xB1u8; 32], false);
    });
    env.deletion
        .set_failpoint(Some(PurgeFailpoint::StopAfterBatches(0)));
    let mut events = env.deletion.subscribe();
    let outcome = start_delete(&env.deletion, vault).await;
    let job_id = outcome.job().job_id.expect("job id");
    let (_, terminal) = drain_until_terminal(&mut events, vault).await;
    assert!(matches!(terminal, DeleteEvent::Stopped { .. }));
    assert_eq!(
        registry_state(&env.fx, vault),
        Some(LifecycleState::Deleting)
    );

    // "崩溃"：作业停了、句柄关掉，持久状态只剩 DELETING。
    env.deletion.shutdown().await;
    let Env {
        fx,
        deletion,
        state,
        app,
    } = env;
    drop(app);
    drop(state);
    drop(deletion);
    let (path, db) = restart_db(fx);

    // 启动恢复：绑定 listener 之前加载 fence + 重新入队 DELETING。
    let deletion = Arc::new(Deletion::with_limits(db.clone(), small_limits()));
    let mut events = deletion.subscribe();
    let report = deletion.recover_startup().await.expect("recover");
    assert_eq!(report.resumed, vec![vault], "DELETING 必须续跑");
    assert!(deletion.admission().is_closed(&vault), "启动即围栏");
    let (_, terminal) = drain_until_terminal(&mut events, vault).await;
    assert!(
        matches!(terminal, DeleteEvent::Deleted { .. }),
        "{terminal:?}"
    );
    let row = registry_row_from(&db, vault).expect("row");
    assert_eq!(row.state, LifecycleState::Deleted);
    assert_eq!(row.delete_job_id, Some(job_id), "续跑复用同一 job");
    assert_no_residue_from(&db, vault);
    deletion.shutdown().await;
    db.close();
    support::cleanup_db_dir(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_after_batches_resumes_from_persisted_progress() {
    let env = Env::new("del-recover-batches", small_limits());
    let vault = env.vault();
    block(|| seed_log_rows(&env.fx, env.scope(), 40, 128));
    env.deletion
        .set_failpoint(Some(PurgeFailpoint::StopAfterBatches(3)));
    let mut events = env.deletion.subscribe();
    let outcome = start_delete(&env.deletion, vault).await;
    let job_id = outcome.job().job_id.expect("job id");
    let (seen, terminal) = drain_until_terminal(&mut events, vault).await;
    assert!(matches!(terminal, DeleteEvent::Stopped { .. }));
    let batches = seen
        .iter()
        .filter(|event| matches!(event, DeleteEvent::Batch { .. }))
        .count();
    assert_eq!(batches, 3, "stop 前恰好 3 批");
    let progress = registry_row(&env.fx, vault).expect("row").delete_progress;
    assert!(progress > 0, "批次进度必须持久化");
    assert!(log_rows(&env.fx, vault) < 40, "批次已经删掉了一部分行");

    env.deletion.shutdown().await;
    let Env {
        fx,
        deletion,
        state,
        app,
    } = env;
    drop(app);
    drop(state);
    drop(deletion);
    let (path, db) = restart_db(fx);
    let deletion = Arc::new(Deletion::with_limits(db.clone(), small_limits()));
    let mut events = deletion.subscribe();
    deletion.recover_startup().await.expect("recover");
    let (_, terminal) = drain_until_terminal(&mut events, vault).await;
    assert!(
        matches!(terminal, DeleteEvent::Deleted { .. }),
        "{terminal:?}"
    );
    let row = registry_row_from(&db, vault).expect("row");
    assert_eq!(row.state, LifecycleState::Deleted);
    assert_eq!(row.delete_job_id, Some(job_id));
    assert_eq!(row.delete_progress, PURGE_ORDER.len() as u64);
    assert_no_residue_from(&db, vault);
    deletion.shutdown().await;
    db.close();
    support::cleanup_db_dir(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_after_terminal_keeps_tombstone_fenced_and_validates_inconsistencies() {
    // 正常删完 → 重启：墓碑 + fence 仍在，不产生新作业。
    let env = Env::new("del-recover-terminal", small_limits());
    let vault = env.vault();
    block(|| seed_log_rows(&env.fx, env.scope(), 6, 64));
    let mut events = env.deletion.subscribe();
    let outcome = start_delete(&env.deletion, vault).await;
    let job_id = outcome.job().job_id.expect("job id");
    let (_, terminal) = drain_until_terminal(&mut events, vault).await;
    assert!(matches!(terminal, DeleteEvent::Deleted { .. }));
    env.deletion.shutdown().await;
    let Env {
        fx,
        deletion,
        state,
        app,
    } = env;
    drop(app);
    drop(state);
    drop(deletion);
    let (path, db) = restart_db(fx);
    let deletion = Arc::new(Deletion::with_limits(db.clone(), small_limits()));
    let report = deletion.recover_startup().await.expect("recover");
    assert!(report.resumed.is_empty(), "DELETED 不得重启作业");
    assert_eq!(report.deleted, 1);
    assert!(deletion.admission().is_closed(&vault), "墓碑必须保持围栏");
    let row = registry_row_from(&db, vault).expect("row");
    assert_eq!(row.state, LifecycleState::Deleted);
    assert_eq!(row.delete_job_id, Some(job_id));
    assert_no_residue_from(&db, vault);

    // 状态矛盾 fail closed：DELETED 墓碑却仍有租户行（在**同一**库上直接破坏 registry）。
    let db2 = db.clone();
    let vault2 = [0xC1u8; 16];
    block(|| {
        db2.write_instance_blocking(move |conn| {
            conn.execute(
                "INSERT INTO vault_registry(vault_id, state, created_at, deleted_at, delete_job_id,
                                        delete_progress)
             VALUES (?1, 'DELETED', 0, 0, ?2, 26)",
                (vault2.as_slice(), [0xC2u8; 16].as_slice()),
            )?;
            conn.execute(
                "INSERT INTO log(vault_id, relay_seq, kind, epoch, event, created_at)
             VALUES (?1, 1, 'update', 1, X'00', 0)",
                [vault2.as_slice()],
            )?;
            Ok(())
        })
    })
    .expect("seed inconsistent tombstone");
    let err = deletion
        .recover_startup()
        .await
        .expect_err("DELETED 仍有租户行必须 fail closed");
    assert!(matches!(err, relay::Error::Invariant(_)), "{err:?}");

    // DELETING 缺 delete_job_id 同样 fail closed。
    let db3 = db.clone();
    let vault3 = [0xC3u8; 16];
    block(|| {
        db3.write_instance_blocking(move |conn| {
            conn.execute(
                "INSERT INTO vault_registry(vault_id, state, created_at, deleted_at)
                 VALUES (?1, 'DELETING', 0, 0)",
                [vault3.as_slice()],
            )?;
            Ok(())
        })
    })
    .expect("seed inconsistent deleting");
    let err = deletion
        .recover_startup()
        .await
        .expect_err("DELETING 缺 job id 必须 fail closed");
    assert!(matches!(err, relay::Error::Invariant(_)), "{err:?}");
    deletion.shutdown().await;
    db.close();
    support::cleanup_db_dir(&path);
}

fn registry_row_from(db: &Db, vault: Id16) -> Option<registry::RegistryRow> {
    db.read_instance(move |conn| registry::load_raw(conn, &vault))
        .expect("registry row")
}

fn assert_no_residue_from(db: &Db, vault: Id16) {
    let leftovers: Vec<(String, i64)> = PURGE_ORDER
        .iter()
        .map(|table| {
            let table = (*table).to_string();
            let count = db
                .read_instance({
                    let table = table.clone();
                    move |conn| {
                        Ok(conn.query_row(
                            &format!("SELECT COUNT(*) FROM {table} WHERE vault_id = ?1"),
                            [vault.as_slice()],
                            |row| row.get(0),
                        )?)
                    }
                })
                .expect("count");
            (table, count)
        })
        .filter(|(_, count)| *count != 0)
        .collect();
    assert!(leftovers.is_empty(), "零残留失败：{leftovers:?}");
}

// ---------------------------------------------------------------- 7) 分批/其它租户/WAL

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_purge_is_batched_keeps_other_tenants_alive_and_bounds_wal() {
    let limits = PurgeLimits {
        rows_per_batch: 64,
        chunk_rows: 8,
        // 真实字节预算：每批 ≤ 8KiB + 单行（行 2KiB，所以通常 4 行/批）。
        bytes_per_batch: 8 * 1024,
        millis_per_batch: 25,
        wal_budget_bytes: 16 * 1024,
    };
    let env = Env::new("del-batched", limits);
    let vault = env.vault();
    block(|| {
        // 800KiB 级的数据量（400 行 × 2KiB），单批 8 行 → 必须多批、WAL 必须被 checkpoint 约束。
        seed_log_rows(&env.fx, env.scope(), 400, 2048);
        create_second_vault(&env.fx);
        seed_log_rows(&env.fx, VaultScope::new(VAULT_B), 10, 64);
    });
    let mut events = env.deletion.subscribe();
    let started = start_delete(&env.deletion, vault).await;
    assert!(started.started());

    // 与 purge 并行：其它租户反复读写，每次都有界（2s 只是"不许被长事务卡住"的上界）。
    let db = env.fx.db.inner().clone();
    let scope_b = VaultScope::new(VAULT_B);
    let other_ops = async move {
        for seq in 0..12i64 {
            let db2 = db.clone();
            tokio::time::timeout(
                Duration::from_secs(2),
                db2.write(scope_b, move |conn| {
                    conn.execute(
                        "INSERT INTO dedup(vault_id, scope, id, request_hash, accepted_seq,
                                           response, created_at)
                         VALUES (?1, 'other/tenant', ?2, ?3, NULL, X'00', ?4)",
                        (
                            scope_b.as_slice(),
                            (seq as u64).to_le_bytes(),
                            [0x11u8; 32].as_slice(),
                            seq,
                        ),
                    )?;
                    Ok(())
                }),
            )
            .await
            .expect("其它租户写被长事务阻塞（超时）")
            .expect("其它租户写失败");
            let db3 = db.clone();
            tokio::time::timeout(Duration::from_secs(2), async move {
                db3.read(scope_b, |conn| {
                    Ok(conn
                        .query_row("SELECT COUNT(*) FROM dedup", [], |row| row.get::<_, i64>(0))?)
                })
            })
            .await
            .expect("其它租户读被长事务阻塞（超时）")
            .expect("其它租户读失败");
            // 让 purge 任务拿到执行权（不是 sleep；纯调度让位）。
            tokio::task::yield_now().await;
        }
    };
    let (_, (seen, terminal)) = tokio::join!(other_ops, async {
        drain_until_terminal(&mut events, vault).await
    });
    assert!(
        matches!(terminal, DeleteEvent::Deleted { .. }),
        "{terminal:?}"
    );

    // 行预算硬上界 + 确实分成多批。
    let batches: Vec<(u64, u64, u64, bool)> = seen
        .iter()
        .filter_map(|event| match event {
            DeleteEvent::Batch {
                rows,
                bytes,
                wal_bytes,
                wal_checkpointed,
                ..
            } => Some((*rows, *bytes, *wal_bytes, *wal_checkpointed)),
            _ => None,
        })
        .collect();
    assert!(
        batches.len() > 10,
        "800KiB 数据必须分成多批：{}",
        batches.len()
    );
    for (rows, bytes, _, _) in &batches {
        assert!(
            *rows <= limits.rows_per_batch as u64,
            "单批行数必须 ≤ rows_per_batch：{rows}"
        );
        assert!(
            *bytes <= limits.bytes_per_batch + 2048,
            "单批字节必须 ≤ bytes_per_batch + 单行：{bytes}"
        );
    }
    let max_wal = batches.iter().map(|(_, _, wal, _)| *wal).max().unwrap_or(0);
    let total_deleted_bytes: u64 = batches.iter().map(|(_, bytes, _, _)| *bytes).sum();
    assert!(
        total_deleted_bytes >= 400 * 2048,
        "字节口径必须覆盖全部 log 行：{total_deleted_bytes}"
    );
    let total_data = 400 * 2048;
    assert!(
        max_wal < 512 * 1024,
        "purge 期间 WAL 不得随总删除量线性增长：max={max_wal} total_data={total_data}"
    );
    assert!(
        batches.iter().any(|(_, _, _, checkpointed)| *checkpointed),
        "超过 WAL 预算时必须做过 checkpoint"
    );
    assert_no_residue(&env.fx, vault);
    assert_eq!(log_rows(&env.fx, VAULT_B), 10, "其它租户数据不受影响");
}

// ---------------------------------------------------------------- 7b) 单行超字节预算

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn byte_budget_never_crosses_from_child_tables_into_parents() {
    // 单行 2KiB > 预算 1KiB：每批只允许一行（首行许可），且**不能**在 log 还没删完时
    // 跳到后面的父表（否则会撞 FK，作业会退化成 DELETE_FAILED）。
    let limits = PurgeLimits {
        rows_per_batch: 64,
        chunk_rows: 8,
        bytes_per_batch: 1024,
        millis_per_batch: 25,
        wal_budget_bytes: 1024 * 1024,
    };
    let env = Env::new("del-byte-budget", limits);
    let vault = env.vault();
    block(|| seed_log_rows(&env.fx, env.scope(), 10, 2048));
    let mut events = env.deletion.subscribe();
    let outcome = start_delete(&env.deletion, vault).await;
    assert!(outcome.started());
    let (seen, terminal) = drain_until_terminal(&mut events, vault).await;
    assert!(
        matches!(terminal, DeleteEvent::Deleted { .. }),
        "{terminal:?}"
    );
    let batches: Vec<(u64, u64)> = seen
        .iter()
        .filter_map(|event| match event {
            DeleteEvent::Batch { rows, bytes, .. } => Some((*rows, *bytes)),
            _ => None,
        })
        .collect();
    let large: Vec<_> = batches
        .iter()
        .filter(|(_, bytes)| *bytes >= 2048)
        .copied()
        .collect();
    assert_eq!(large.len(), 10, "10 条 2KiB 行必须各自成批：{large:?}");
    for (rows, bytes) in large {
        assert_eq!(rows, 1, "单行超预算时每批只能删一行");
        assert_eq!(bytes, 2048, "字节口径 = event 字节");
    }
    assert!(
        batches.iter().all(|(_, bytes)| *bytes <= 2048),
        "单批字节不得超过单行上界：{batches:?}"
    );
    assert_no_residue(&env.fx, vault);
}

// ---------------------------------------------------------------- 7c) 旋转中/claim

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_purges_in_flight_rotation_and_claim_is_fenced() {
    let env = Env::new("del-rotation", small_limits());
    let vault = env.vault();
    let scope = env.scope();
    // 模拟"轮换进行中"：vaults.state=rotating + 一个 staged 候选 epoch（真实轮换签名链
    // 不属于 S3 的测试面；purge 必须在任意运行时状态下都能把 epoch/rotation 行清干净）。
    block(|| {
        env.fx
            .db
            .inner()
            .write_blocking(scope, move |conn| {
                conn.execute(
                    "UPDATE vaults SET state = 'rotating' WHERE vault_id = ?1",
                    [scope.as_slice()],
                )?;
                conn.execute(
                    "INSERT INTO epochs(vault_id, epoch, state, created_at)
                     VALUES (?1, 2, 'staged', 0)",
                    [scope.as_slice()],
                )?;
                conn.execute(
                    "INSERT INTO rotations(vault_id, rotation_id, from_epoch, to_epoch,
                         freeze_seq, revoked_count, state, created_at, updated_at)
                     VALUES (?1, ?2, 1, 2, 0, 0, 'frozen', 0, 0)",
                    (scope.as_slice(), [0xD1u8; 16].as_slice()),
                )?;
                Ok(())
            })
            .expect("seed rotating state");
    });
    let mut events = env.deletion.subscribe();
    let outcome = start_delete(&env.deletion, vault).await;
    assert!(outcome.started());
    let (_, terminal) = drain_until_terminal(&mut events, vault).await;
    assert!(
        matches!(terminal, DeleteEvent::Deleted { .. }),
        "{terminal:?}"
    );
    assert_no_residue(&env.fx, vault);
    assert_eq!(table_count(&env.fx, "epochs", vault), 0);

    // claim（`/v2/vaults`）没有路径 vault，middleware 不拦；必须由事务级复核拒绝 —— 票据
    // 本身在标记事务里已被作废，但这里断言的是 fence 的 410 语义（不是 403）。
    let sign_pk = env.fx.device_signing_key.verifying_key().to_bytes();
    let request = InitRequest {
        vault_id: vault,
        device_id: env.fx.device,
        sign_pk,
        hpke_pk: [4u8; 32],
        manager_pub: env.fx.manager_pub,
        setup_secret: String::new(),
        certificate: certificate_bytes(&vault, &env.fx.device, &sign_pk, &[4u8; 32], 1),
        certificate_signature: b"certificate-sig".to_vec(),
    };
    let body = relay::wire::encode(vec![
        (0, relay::wire::bytes(vec![0x33u8; 32])),
        (1, relay::wire::bytes(request.vault_id)),
        (2, relay::wire::bytes(request.device_id)),
        (3, relay::wire::bytes(request.sign_pk)),
        (4, relay::wire::bytes(request.hpke_pk)),
        (5, relay::wire::bytes(request.manager_pub)),
        (6, relay::wire::bytes(request.certificate.clone())),
        (7, relay::wire::bytes(request.certificate_signature.clone())),
    ])
    .expect("claim body");
    let reply = call(
        &env.app,
        Request::builder()
            .method("POST")
            .uri("/v2/vaults")
            .header(header::CONTENT_TYPE, relay::wire::CONTENT_TYPE)
            .body(Body::from(body))
            .expect("claim request"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::GONE);
    assert_eq!(reply.error_code(), "VAULT_DELETED");
    assert_eq!(
        registry_state(&env.fx, vault),
        Some(LifecycleState::Deleted)
    );
}

// ---------------------------------------------------------------- 8) admin 触发路由

fn csrf_from_page(body: &str) -> String {
    let marker = "name=\"csrf\" value=\"";
    let start = body
        .find(marker)
        .unwrap_or_else(|| panic!("页面里没有 CSRF token：{body}"))
        + marker.len();
    let rest = &body[start..];
    let end = rest.find('"').expect("csrf 结束引号");
    rest[..end].to_string()
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with(&format!("{}=", relay::admin_web::SESSION_COOKIE)))
        .map(|value| value.split(';').next().expect("cookie pair").to_string())
}

fn admin_get(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::HOST, ADMIN_AUTHORITY);
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::empty()).expect("admin get")
}

fn admin_post(uri: &str, form: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::HOST, ADMIN_AUTHORITY)
        .header(header::ORIGIN, ADMIN_ORIGIN)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder
        .body(Body::from(form.to_string()))
        .expect("admin post")
}

async fn admin_call(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_delete_route_requires_session_csrf_and_confirm() {
    let env = Env::new("del-admin", small_limits());
    let vault = env.vault();
    block(|| seed_log_rows(&env.fx, env.scope(), 6, 64));
    let admin_token = "admin-token-for-delete-test";
    let hash = relay::admin_web::token_hash(admin_token);
    let db = env.fx.db.inner().clone();
    block(|| {
        db.write_instance_blocking(move |conn| relay::admin_web::store_token_hash(conn, &hash))
    })
    .expect("store admin token");
    let policy =
        relay::admin_web::AdminHostPolicy::for_listen(ADMIN_AUTHORITY.parse().expect("addr"), &[]);
    let web = relay::admin_web::AdminWeb::new(
        env.fx.db.inner().clone(),
        env.fx.clock.clone(),
        Some(relay::admin_web::token_hash(admin_token)),
        None,
        policy,
        env.deletion.clone(),
    );
    let admin = relay::admin_web::router(web.clone());

    // 未登录 → 303 到登录页。
    let (status, _, _) = admin_call(
        &admin,
        admin_post(
            &format!("/admin/vaults/{}/delete", hex_lower(&vault)),
            "csrf=x&confirm=x",
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(registry_state(&env.fx, vault), Some(LifecycleState::Active));

    // 登录（登录页 CSRF + token）。
    let (status, _, page) = admin_call(&admin, admin_get("/admin/login", None)).await;
    assert_eq!(status, StatusCode::OK);
    let login_csrf = csrf_from_page(&page);
    let (status, headers, _) = admin_call(
        &admin,
        admin_post(
            "/admin/login",
            &format!("csrf={login_csrf}&token={admin_token}"),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let cookie = session_cookie(&headers).expect("session cookie");

    // 管理页的 CSRF（会话绑定）。
    let (status, _, page) = admin_call(&admin, admin_get("/admin", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    let csrf = csrf_from_page(&page);
    let delete_uri = format!("/admin/vaults/{}/delete", hex_lower(&vault));

    // 错 CSRF / 错 confirm 都不改状态。
    let (status, _, _) = admin_call(
        &admin,
        admin_post(
            &delete_uri,
            &format!("csrf=bad&confirm=DELETE+{}", hex_lower(&vault)),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = admin_call(
        &admin,
        admin_post(
            &delete_uri,
            &format!("csrf={csrf}&confirm=deadbeef"),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(registry_state(&env.fx, vault), Some(LifecycleState::Active));

    // 删除前先签一个 token（删除态连 challenge 都会被拒，不能再签）。
    let token = block(|| env.fx.device_token());
    // 正确请求 → 202，且返回时 registry 已是持久 DELETING（响应后数据面立即 410）。
    // 先停在第 0 批之前，避免后台 purge 在断言前删完（无 sleep 竞态）。
    env.deletion
        .set_failpoint(Some(PurgeFailpoint::StopAfterBatches(0)));
    let mut events = env.deletion.subscribe();
    let (status, _, page) = admin_call(
        &admin,
        admin_post(
            &delete_uri,
            &format!("csrf={csrf}&confirm=DELETE+{}", hex_lower(&vault)),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{page}");
    assert!(page.contains("DELETING"), "作业页应显示状态：{page}");
    let row = registry_row(&env.fx, vault).expect("row");
    assert_eq!(row.state, LifecycleState::Deleting);
    assert!(row.delete_job_id.is_some());
    let reply = call(&env.app, state_request("/v2", vault, &token)).await;
    assert_eq!(reply.status, StatusCode::GONE);
    let (_, stopped) = drain_until_terminal(&mut events, vault).await;
    assert!(matches!(stopped, DeleteEvent::Stopped { .. }));

    // 重复 POST（同一页面 CSRF）仍 202 + 同一 job（幂等）。会话 CSRF 不随请求消耗。
    let (status, _, page) = admin_call(
        &admin,
        admin_post(
            &delete_uri,
            &format!("csrf={csrf}&confirm=DELETE+{}", hex_lower(&vault)),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{page}");
    assert_eq!(
        registry_row(&env.fx, vault).expect("row").delete_job_id,
        row.delete_job_id
    );
    // 重复 POST 只返回同一 job，不得再起第二个任务/再发 Stopped。
    match events.try_recv() {
        Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
        other => panic!("重复 POST 不应产生新作业事件: {other:?}"),
    }

    // 解除 failpoint 并让作业跑完。
    env.deletion.shutdown().await;
    env.deletion.set_failpoint(None);
    let resumed = start_delete(&env.deletion, vault).await;
    assert!(matches!(resumed, StartOutcome::AlreadyDeleting(_)));
    let (_, terminal) = drain_until_terminal(&mut events, vault).await;
    assert!(matches!(terminal, DeleteEvent::Deleted { .. }));
    assert_no_residue(&env.fx, vault);

    // 已完成后再 POST：仍是 202（幂等墓碑，不再有作业）。
    let (status, _, page) = admin_call(
        &admin,
        admin_post(
            &delete_uri,
            &format!("csrf={csrf}&confirm=DELETE+{}", hex_lower(&vault)),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{page}");
    assert!(page.contains("DELETED"), "墓碑页应显示 DELETED：{page}");
}

// ---------------------------------------------------------------- 9) RESERVED/终态语义

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_only_applies_to_active_vaults() {
    let env = Env::new("del-states", small_limits());
    let vault = env.vault();
    // RESERVED：没有租户数据，必须先 cancel；删除请求 409 且不改 registry。
    let reserved = block(|| {
        relay::admin::create_reservation(fx_db(&env.fx), Duration::from_secs(900))
            .expect("reservation")
            .vault_id
    });
    let err = env
        .deletion
        .start(reserved)
        .await
        .expect_err("RESERVED 不能直接删除");
    assert_eq!(err.code(), Some(ErrorCode::CasConflict));
    assert_eq!(
        registry_state(&env.fx, reserved),
        Some(LifecycleState::Reserved)
    );
    // 取消后（CANCELLED）再删除 → 409（没有可删除的租户数据，ID 也不可复用）。
    block(|| registry::cancel_reservation(fx_db(&env.fx), reserved)).expect("cancel");
    let err = env
        .deletion
        .start(reserved)
        .await
        .expect_err("CANCELLED 不能删除");
    assert_eq!(err.code(), Some(ErrorCode::InvalidState));
    assert_eq!(
        registry_state(&env.fx, reserved),
        Some(LifecycleState::Cancelled)
    );
    // 未知 vault → 404。
    let err = env
        .deletion
        .start([0xEEu8; 16])
        .await
        .expect_err("unknown vault");
    assert_eq!(err.code(), Some(ErrorCode::NotFound));
    // ACTIVE 本体仍可正常删除（控制组）。
    let mut events = env.deletion.subscribe();
    let outcome = start_delete(&env.deletion, vault).await;
    assert!(outcome.started());
    let (_, terminal) = drain_until_terminal(&mut events, vault).await;
    assert!(matches!(terminal, DeleteEvent::Deleted { .. }));
}

fn fx_db(fx: &Fixture) -> &Db {
    fx.db.inner()
}
