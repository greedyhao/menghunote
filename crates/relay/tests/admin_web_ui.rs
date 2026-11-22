//! ADR 0018 **S4a**：最小 Web 管理页面（列表/详情/创建领取链接/取消预留/删除确认与 202 进度）。
//!
//! 覆盖任务书 §4：
//! 1. 登录后列表空/ACTIVE/RESERVED/DELETING/DELETED、稳定排序；详情字段与 HTML 转义；
//! 2. 创建：CSRF 正/负、自动 id/900s、链接可解析（relay 端结构 + 票据哈希回查）、
//!    刷新后链接不再出现、无 `public_url` 明确报错；
//! 3. 取消：RESERVED 成功/ACTIVE 409/重复幂等/EXPIRED 409（claim 竞争语义沿用 S2）；
//! 4. 删除：确认字符串严格、CSRF、202 页/进度、重复同 job、非 ACTIVE 409、
//!    列表显示 DELETING/DELETED；
//! 5. 权限：未登录 POST 不创建/不删除；页面/一次性链接不包含 session/ticket 泄漏。
//!
//! 全部真实 axum router + 临时 SQLite + `FakeClock`，**无 sleep**（删除收敛靠
//! `broadcast` 作业事件；其它断言只看持久状态）。

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use tokio::sync::broadcast;
use tower::ServiceExt;

use relay::admin;
use relay::admin_web::{self, AdminHostPolicy, AdminWeb, SESSION_COOKIE};
use relay::config::PublicUrl;
use relay::db::{Db, DbConfig};
use relay::deletion::{DeleteEvent, Deletion, PurgeFailpoint, PurgeLimits};
use relay::ids::{b64url_decode, hex_lower, parse_id16_text, Id16};
use relay::registry::{self, LifecycleState};
use relay::time::{Clock, FakeClock};
use support::relay;
use support::Fixture;

const BASE_TIME: i64 = 1_800_000_000;
const ADMIN_TOKEN: &str = "admin-ui-s4a-token";
const ADMIN_AUTHORITY: &str = "127.0.0.1:18081";
const ADMIN_ORIGIN: &str = "http://127.0.0.1:18081";
const DATA_PUBLIC_URL: &str = "http://127.0.0.1:18080";
const CONVERGE: Duration = Duration::from_secs(30);

fn block<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}

fn admin_addr() -> std::net::SocketAddr {
    ADMIN_AUTHORITY.parse().expect("admin authority")
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

// ---------------------------------------------------------------- 环境

/// 临时目录清理守卫（声明在 `WebEnv` 最后，保证 DB 句柄先关闭）。
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        // `cleanup_db_dir` 接收 **db 文件路径** 并删它的父目录；不能把目录本身传进去
        // （否则会删掉 CARGO_TARGET_TMPDIR，误伤并行运行的其他用例）。
        support::cleanup_db_dir(&self.0);
    }
}

/// 一个真实 admin listener 环境：`Db` + `AdminWeb` + 真实 axum router。
struct WebEnv {
    db: Db,
    clock: Arc<FakeClock>,
    deletion: Arc<Deletion>,
    web: Arc<AdminWeb>,
    app: Router,
    token: String,
    /// 最后 drop：Fixture/清理守卫必须在 DB 句柄之后。
    fixture: Option<Fixture>,
    _cleanup: Option<Cleanup>,
}

impl WebEnv {
    fn empty(label: &str, public_url: Option<&str>) -> Self {
        let path = support::temp_db_path(label);
        let clock = Arc::new(FakeClock::new(BASE_TIME));
        let db = Db::open(DbConfig::new(path.clone()), clock.clone()).expect("open db");
        Self::build(db, clock, None, Some(Cleanup(path)), public_url)
    }

    fn active(label: &str, public_url: Option<&str>) -> Self {
        let fixture = Fixture::new(label);
        let db = fixture.db.inner().clone();
        let clock = fixture.clock.clone();
        Self::build(db, clock, Some(fixture), None, public_url)
    }

    fn build(
        db: Db,
        clock: Arc<FakeClock>,
        fixture: Option<Fixture>,
        cleanup: Option<Cleanup>,
        public_url: Option<&str>,
    ) -> Self {
        let hash = admin_web::token_hash(ADMIN_TOKEN);
        db.write_instance_blocking(move |conn| admin_web::store_token_hash(conn, &hash))
            .expect("store admin token hash");
        let public = public_url.map(|text| PublicUrl::parse(text).expect("public_url"));
        let policy = AdminHostPolicy::for_listen(admin_addr(), &[]);
        let deletion = Arc::new(Deletion::with_limits(db.clone(), small_limits()));
        let web = AdminWeb::new(
            db.clone(),
            clock.clone(),
            Some(admin_web::token_hash(ADMIN_TOKEN)),
            public,
            policy,
            deletion.clone(),
        );
        let app = admin_web::router(web.clone());
        Self {
            db,
            clock,
            deletion,
            web,
            app,
            token: ADMIN_TOKEN.to_string(),
            fixture,
            _cleanup: cleanup,
        }
    }

    fn fixture_vault(&self) -> Id16 {
        self.fixture
            .as_ref()
            .map(|fixture| fixture.vault_id)
            .expect("empty env 没有 ACTIVE vault")
    }

    fn reserved(&self) -> admin::IssuedCreateTicket {
        admin::create_reservation(&self.db, Duration::from_secs(900)).expect("reservation")
    }

    fn state_of(&self, vault: Id16) -> Option<LifecycleState> {
        self.db
            .read_instance(move |conn| registry::state_raw(conn, &vault))
            .expect("registry state")
    }

    fn list(&self) -> Vec<registry::VaultListEntry> {
        let now = self.clock.now_secs();
        self.db
            .read_instance(move |conn| registry::list_vaults(conn, now))
            .expect("list vaults")
    }
}

// ---------------------------------------------------------------- 请求/响应辅助

fn get_req(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::HOST, ADMIN_AUTHORITY);
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::empty()).expect("request")
}

fn post_form_req(uri: &str, form: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::HOST, ADMIN_AUTHORITY)
        .header(header::ORIGIN, ADMIN_ORIGIN)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::from(form.to_string())).expect("request")
}

async fn call(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

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
        .find(|value| value.starts_with(&format!("{SESSION_COOKIE}=")))
        .map(|value| value.split(';').next().expect("cookie pair").to_string())
}

fn location(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
}

/// 登录并返回会话 cookie。
async fn login(app: &Router) -> String {
    let (status, _, page) = call(app, get_req("/admin/login", None)).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let csrf = csrf_from_page(&page);
    let (status, headers, _) = call(
        app,
        post_form_req(
            "/admin/login",
            &format!("csrf={csrf}&token={ADMIN_TOKEN}"),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "登录必须成功");
    session_cookie(&headers).expect("session cookie")
}

/// 从 claim 链接取票据与 vault id（纯本地解析，不依赖 sync crate 的依赖方向）。
fn ticket_of_link(link: &str) -> [u8; 32] {
    let (_, fragment) = link.split_once('#').expect("fragment");
    let encoded = fragment.strip_prefix("ticket=").expect("ticket 字段");
    b64url_decode(encoded)
        .expect("base64url")
        .as_slice()
        .try_into()
        .expect("32B ticket")
}

fn vault_of_link(link: &str) -> Id16 {
    let (head, _) = link.split_once('#').expect("fragment");
    let text = head
        .split_once("vault=")
        .expect("vault 字段")
        .1
        .split('&')
        .next()
        .expect("vault 值");
    parse_id16_text(text).expect("vault id")
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
                    panic!("删除事件订阅落后 {skipped} 条")
                }
                Err(err) => panic!("删除事件通道关闭：{err}"),
            }
        }
    })
    .await
    .expect("等待删除作业终态超时")
}

fn contains_before(body: &str, first: &str, second: &str) {
    let first_at = body
        .find(first)
        .unwrap_or_else(|| panic!("页面缺少 {first}：{body}"));
    let second_at = body
        .find(second)
        .unwrap_or_else(|| panic!("页面缺少 {second}：{body}"));
    assert!(
        first_at < second_at,
        "列表排序必须稳定（{first} 应在 {second} 之前）"
    );
}

// ---------------------------------------------------------------- 1) 列表

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_page_is_friendly_when_empty_and_requires_login() {
    let env = block(|| WebEnv::empty("s4a-empty", Some(DATA_PUBLIC_URL)));
    let app = env.app.clone();

    // 未登录：所有页面重定向到登录页；POST 不会创建任何预留。
    for uri in ["/admin", "/admin/vaults/new", "/admin/vaults/deadbeef"] {
        let (status, headers, _) = call(&app, get_req(uri, None)).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{uri}");
        assert_eq!(location(&headers), Some("/admin/login"), "{uri}");
    }
    let (status, headers, _) = call(&app, post_form_req("/admin/vaults", "csrf=x", None)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/login"));
    assert!(env.list().is_empty(), "未登录 POST 不得创建预留");

    // 登录后：空 relay 友好文案 + 数据面 URL 只读展示 + 安全响应头。
    let cookie = login(&app).await;
    let (status, headers, body) = call(&app, get_req("/admin", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("已登录"), "{body}");
    assert!(body.contains("vault 数：<code>0</code>"), "{body}");
    assert!(body.contains("空库"), "空列表必须有友好文案：{body}");
    assert!(body.contains(DATA_PUBLIC_URL), "必须显示数据面 URL：{body}");
    assert!(
        body.contains("只读，来自配置 <code>public_url</code>"),
        "{body}"
    );
    assert!(body.contains("退出登录"), "{body}");
    assert!(body.contains("创建领取链接"), "{body}");
    assert!(!body.contains(&env.token), "页面不得回显 admin token");
    assert!(!body.contains(&cookie), "页面不得回显会话 cookie 值");
    assert_eq!(
        headers
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    assert_eq!(
        headers
            .get(header::X_FRAME_OPTIONS)
            .and_then(|v| v.to_str().ok()),
        Some("DENY")
    );

    // 未知路径：404 且不泄露内部路径/临时库位置。
    let (status, _, body) = call(&app, get_req("/admin/nope", Some(&cookie))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!body.contains("relay.sqlite"), "{body}");
    assert!(!body.contains("sqlite"), "{body}");

    // 未配置 public_url：列表明确标注（创建页另有 409，见创建用例）。
    let env = block(|| WebEnv::empty("s4a-empty-no-url", None));
    let app = env.app.clone();
    let cookie = login(&app).await;
    let (status, _, body) = call(&app, get_req("/admin", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("未配置（无法生成领取链接）"), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_page_shows_active_reserved_deleting_and_deleted_rows() {
    let env = block(|| WebEnv::active("s4a-list", Some(DATA_PUBLIC_URL)));
    let app = env.app.clone();
    let client = login(&app).await;
    let active = env.fixture_vault();
    let active_hex = hex_lower(&active);

    // 1) ACTIVE（夹具 vault）+ RESERVED（新预留，时钟推进保证稳定排序）。
    env.clock.advance_secs(10);
    let reserved = block(|| env.reserved());
    env.clock.advance_secs(10);
    let reserved_hex = hex_lower(&reserved.vault_id);
    let (status, _, body) = call(&app, get_req("/admin", Some(&client))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("<th>数据面 URL</th>"), "{body}");
    assert!(body.contains("<th>设备数</th>"), "{body}");
    assert!(body.contains("<th>票据过期</th>"), "{body}");
    assert!(body.contains("<th>删除进度</th>"), "{body}");
    assert!(body.contains(&active_hex), "{body}");
    assert!(body.contains(&reserved_hex), "{body}");
    assert!(body.contains("ACTIVE"), "{body}");
    assert!(body.contains("RESERVED"), "{body}");
    assert!(
        body.contains("<code>1</code>"),
        "ACTIVE vault 设备数应为 1：{body}"
    );
    // 稳定排序：先创建（fixture）的行在前。
    contains_before(&body, &active_hex, &reserved_hex);
    // 数据面 URL 是全局只读配置值，出现在行/表头。
    assert!(body.matches(DATA_PUBLIC_URL).count() >= 2, "{body}");

    // 2) DELETING：先停在第 0 批之前，避免后台 purge 在断言前删完（无 sleep 竞态）。
    env.deletion
        .set_failpoint(Some(PurgeFailpoint::StopAfterBatches(0)));
    let mut events = env.deletion.subscribe();
    let outcome = env.deletion.start(active).await.expect("start delete");
    assert!(outcome.started());
    let (_, stopped) = drain_until_terminal(&mut events, active).await;
    assert!(matches!(stopped, DeleteEvent::Stopped { .. }));
    let (status, _, body) = call(&app, get_req("/admin", Some(&client))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("DELETING"), "{body}");
    assert!(body.contains("<code>0/26</code>"), "{body}");
    assert!(body.contains("RESERVED"), "其它 vault 不受影响：{body}");
    contains_before(&body, &active_hex, &reserved_hex);

    // 3) DELETED：解除 failpoint 并让同一 job 跑完。
    env.deletion.shutdown().await;
    env.deletion.set_failpoint(None);
    let resumed = env.deletion.start(active).await.expect("resume delete");
    assert!(!resumed.started(), "重启的是同一 job（AlreadyDeleting）");
    let (_, terminal) = drain_until_terminal(&mut events, active).await;
    assert!(matches!(terminal, DeleteEvent::Deleted { .. }));
    let (status, _, body) = call(&app, get_req("/admin", Some(&client))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("DELETED"), "{body}");
    assert!(body.contains("<code>26/26</code>"), "{body}");
}

// ---------------------------------------------------------------- 2) 详情

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detail_page_separates_lifecycle_authz_devices_tickets_and_usage() {
    let env = block(|| WebEnv::active("s4a-detail", Some(DATA_PUBLIC_URL)));
    let app = env.app.clone();
    let client = login(&app).await;
    let active = env.fixture_vault();
    let active_hex = hex_lower(&active);

    let (status, _, body) = call(
        &app,
        get_req(&format!("/admin/vaults/{active_hex}"), Some(&client)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for section in ["生命周期", "授权 / 运行", "设备与票据", "用量"] {
        assert!(body.contains(section), "缺少小节 {section}：{body}");
    }
    assert!(body.contains(&active_hex), "{body}");
    assert!(body.contains("ACTIVE"), "{body}");
    assert!(body.contains("legacy"), "授权状态必须显示：{body}");
    assert!(body.contains("active epoch"), "{body}");
    assert!(body.contains("control rev"), "{body}");
    assert!(body.contains("head seq"), "{body}");
    assert!(body.contains("设备数"), "{body}");
    assert!(body.contains("<code>1</code> 台"), "{body}");
    assert!(body.contains("票据总数"), "{body}");
    assert!(body.contains("仍可领取票据"), "{body}");
    assert!(body.contains("普通配额已用（字节）"), "{body}");
    assert!(body.contains("维护配额已用（字节）"), "{body}");
    assert!(body.contains("数据面 URL"), "{body}");
    assert!(body.contains(DATA_PUBLIC_URL), "{body}");
    // 不显示 ticket hash/明文；详情里没有可领取链接。
    assert!(!body.contains("token_hash"), "{body}");
    assert!(!body.contains("mn-pair://"), "详情不得出现领取链接：{body}");
    assert!(!body.contains(&client), "{body}");

    // RESERVED 详情：没有租户字段（授权/epoch 显示 —），有取消表单、无删除入口。
    let reserved = block(|| env.reserved());
    let reserved_hex = hex_lower(&reserved.vault_id);
    let (status, _, body) = call(
        &app,
        get_req(&format!("/admin/vaults/{reserved_hex}"), Some(&client)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("RESERVED"), "{body}");
    assert!(
        body.contains(&format!("action=\"/admin/vaults/{reserved_hex}/cancel\"")),
        "RESERVED 详情必须有取消表单：{body}"
    );
    assert!(
        !body.contains("删除该 vault"),
        "RESERVED 不得出现删除入口：{body}"
    );
    assert!(body.contains("取消预留会立即作废"), "{body}");
    // 未配置 public_url 时详情只读展示"未配置"。
    assert!(!body.contains("ticket="), "{body}");

    // 未知 / 非法 id：404，不回显路径。
    for uri in [
        "/admin/vaults/deadbeef",
        "/admin/vaults/00000000000000000000000000000000",
    ] {
        let (status, _, body) = call(&app, get_req(uri, Some(&client))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert!(!body.contains("relay.sqlite"), "{body}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detail_and_delete_pages_escape_registry_error_text() {
    let env = block(|| WebEnv::active("s4a-escape", Some(DATA_PUBLIC_URL)));
    let app = env.app.clone();
    let client = login(&app).await;
    let vault = env.fixture_vault();
    let vault_hex = hex_lower(&vault);

    // 让作业在 0 批后失败（DELETE_FAILED），再把错误文本替换成恶意 HTML（模拟被污染的运维文本）。
    env.deletion
        .set_failpoint(Some(PurgeFailpoint::FailAfterBatches(0)));
    let mut events = env.deletion.subscribe();
    env.deletion.start(vault).await.expect("start delete");
    let (_, failed) = drain_until_terminal(&mut events, vault).await;
    assert!(matches!(failed, DeleteEvent::Failed { .. }));
    block(|| {
        env.db
            .write_instance_blocking(move |conn| {
                conn.execute(
                    "UPDATE vault_registry SET delete_error = ?1 WHERE vault_id = ?2",
                    ("<script>alert('x')</script> & \"quoted\"", vault.as_slice()),
                )?;
                Ok(())
            })
            .expect("inject malicious error text");
    });

    let (status, _, body) = call(
        &app,
        get_req(&format!("/admin/vaults/{vault_hex}"), Some(&client)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("DELETE_FAILED"), "{body}");
    assert!(
        body.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; &quot;quoted&quot;"),
        "错误文本必须完整转义：{body}"
    );
    assert!(!body.contains("<script>"), "不得输出原始脚本标签：{body}");

    let (status, _, body) = call(
        &app,
        get_req(&format!("/admin/vaults/{vault_hex}/delete"), Some(&client)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("<script>"), "{body}");
    assert!(body.contains("&lt;script&gt;"), "{body}");
}

// ---------------------------------------------------------------- 3) 创建领取链接

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_page_requires_public_url_and_never_generates_unusable_links() {
    let env = block(|| WebEnv::empty("s4a-create-no-url", None));
    let app = env.app.clone();
    let client = login(&app).await;

    let (status, _, body) = call(&app, get_req("/admin/vaults/new", Some(&client))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("public_url"), "{body}");
    assert!(body.contains("未配置"), "{body}");
    assert!(
        !body.contains("action=\"/admin/vaults\""),
        "未配置时不得保留可提交表单：{body}"
    );
    // 409 页没有表单，CSRF 从列表页取。
    let (_, _, home) = call(&app, get_req("/admin", Some(&client))).await;
    let csrf = csrf_from_page(&home);
    let (status, _, body) = call(
        &app,
        post_form_req("/admin/vaults", &format!("csrf={csrf}"), Some(&client)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("public_url"), "{body}");
    assert!(
        env.list().is_empty(),
        "未配置 public_url 时不得创建预留/票据"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_reveals_claim_link_exactly_once_with_900s_ttl() {
    let env = block(|| WebEnv::empty("s4a-create", Some(DATA_PUBLIC_URL)));
    let app = env.app.clone();
    let client = login(&app).await;

    // 表单：没有 vault_id/TTL/quota 输入。
    let (status, _, page) = call(&app, get_req("/admin/vaults/new", Some(&client))).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("action=\"/admin/vaults\""), "{page}");
    assert!(page.contains(DATA_PUBLIC_URL), "{page}");
    assert!(!page.contains("name=\"vault_id\""), "{page}");
    assert!(!page.contains("name=\"ttl\""), "{page}");
    assert!(!page.contains("name=\"quota\""), "{page}");
    let csrf = csrf_from_page(&page);

    // CSRF 负例：缺/错 token 都 403，且不产生预留。
    for form in ["", "csrf=wrong"] {
        let (status, _, body) =
            call(&app, post_form_req("/admin/vaults", form, Some(&client))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(body.contains("CSRF"), "{body}");
        assert!(env.list().is_empty(), "CSRF 失败不得创建预留");
    }

    // CSRF 正例：303 到不含票据的 URL（reveal id 是随机的，不是 ticket）。
    let (status, headers, _) = call(
        &app,
        post_form_req("/admin/vaults", &format!("csrf={csrf}"), Some(&client)),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = location(&headers).expect("location").to_string();
    assert!(location.starts_with("/admin/vaults/created/"), "{location}");
    assert!(!location.contains("ticket"), "URL 不得含票据：{location}");
    assert_eq!(env.web.reveals_len(), 1);
    let entries = env.list();
    assert_eq!(entries.len(), 1);
    let reserved = &entries[0];
    assert_eq!(reserved.state, LifecycleState::Reserved);
    assert_eq!(reserved.expires_at, Some(BASE_TIME + 900));

    // 一次性页面：显示链接一次，900s TTL、票据能回查到同一 vault。
    let (status, _, page) = call(&app, get_req(&location, Some(&client))).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(
        page.contains("mn-pair://create?v=1&amp;relay="),
        "领取链接必须以 HTML 转义形式出现：{page}"
    );
    assert!(
        page.contains("relay=http%3A%2F%2F127.0.0.1%3A18080"),
        "链接 relay 必须与配置 canonical 一致：{page}"
    );
    let link_start = page.find("mn-pair://create?v=1&amp;relay=").expect("link");
    let link_end = page[link_start..].find('<').expect("link end") + link_start;
    // 页面是 HTML：`&` 被转义成 `&amp;`，解析回原始链接。
    let link = page[link_start..link_end].replace("&amp;", "&");
    let vault = vault_of_link(&link);
    assert_eq!(vault, reserved.vault_id);
    let ticket = ticket_of_link(&link);
    let row = admin::load_create_ticket(&env.db, &ticket)
        .expect("load ticket")
        .expect("ticket row");
    assert_eq!(row.vault_id, reserved.vault_id);
    assert_eq!(row.quota, 1);
    assert_eq!(row.consumed_at, None);
    assert_eq!(row.expires_at - row.created_at, 900);
    assert_eq!(row.created_at, BASE_TIME);
    assert_eq!(env.web.reveals_len(), 0, "取走后必须删除");

    // 刷新/第二次打开：404，且页面不再出现链接/ticket。
    let (status, _, page) = call(&app, get_req(&location, Some(&client))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{page}");
    assert!(!page.contains("mn-pair://create"), "{page}");
    let encoded = relay::ids::b64url_encode(&ticket);
    assert!(!page.contains(&encoded), "{page}");
    assert!(page.contains("一次性页面已失效"), "{page}");

    // 列表/详情都拿不到一次性链接或 ticket。
    let (_, _, list_body) = call(&app, get_req("/admin", Some(&client))).await;
    assert!(!list_body.contains("mn-pair://"), "{list_body}");
    assert!(!list_body.contains(&encoded), "{list_body}");
    let (_, _, detail_body) = call(
        &app,
        get_req(
            &format!("/admin/vaults/{}", hex_lower(&vault)),
            Some(&client),
        ),
    )
    .await;
    assert!(!detail_body.contains(&encoded), "{detail_body}");
    assert!(!detail_body.contains(&client), "{detail_body}");
}

// ---------------------------------------------------------------- 4) 取消预留

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_reservation_from_ui_and_conflict_semantics() {
    let env = block(|| WebEnv::active("s4a-cancel", Some(DATA_PUBLIC_URL)));
    let app = env.app.clone();
    let client = login(&app).await;
    let active = env.fixture_vault();
    let reserved = block(|| env.reserved());
    let reserved_hex = hex_lower(&reserved.vault_id);

    let detail_uri = format!("/admin/vaults/{reserved_hex}");
    let (status, _, page) = call(&app, get_req(&detail_uri, Some(&client))).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let csrf = csrf_from_page(&page);
    let cancel_uri = format!("{detail_uri}/cancel");

    // 缺 CSRF → 403，状态不变。
    let (status, _, body) = call(&app, post_form_req(&cancel_uri, "csrf=x", Some(&client))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(
        env.state_of(reserved.vault_id),
        Some(LifecycleState::Reserved)
    );

    // 正常取消 → 303 回列表；状态 CANCELLED、票据作废。
    let (status, headers, _) = call(
        &app,
        post_form_req(&cancel_uri, &format!("csrf={csrf}"), Some(&client)),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin"));
    assert_eq!(
        env.state_of(reserved.vault_id),
        Some(LifecycleState::Cancelled)
    );
    let row = admin::load_create_ticket(&env.db, &reserved.token)
        .expect("load ticket")
        .expect("row");
    assert_eq!(row.quota, 0, "取消必须作废票据");
    assert!(row.consumed_at.is_some());

    // 重复取消：幂等（303，状态保持 CANCELLED）。
    let (status, _, body) = call(
        &app,
        post_form_req(&cancel_uri, &format!("csrf={csrf}"), Some(&client)),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    assert_eq!(
        env.state_of(reserved.vault_id),
        Some(LifecycleState::Cancelled)
    );

    // ACTIVE（claim 已赢）→ 409，绝不删除已创建 vault。
    let active_hex = hex_lower(&active);
    let active_detail = format!("/admin/vaults/{active_hex}");
    // ACTIVE 详情没有表单（没有可用的会话 CSRF），从列表页取。
    let (_, _, home) = call(&app, get_req("/admin", Some(&client))).await;
    let csrf = csrf_from_page(&home);
    let (status, _, body) = call(
        &app,
        post_form_req(
            &format!("{active_detail}/cancel"),
            &format!("csrf={csrf}"),
            Some(&client),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("ACTIVE"), "{body}");
    assert_eq!(env.state_of(active), Some(LifecycleState::Active));

    // 过期预留：返回 409 且落 EXPIRED（票据作废）；不会静默删除。
    let expired = block(|| env.reserved());
    env.clock.advance_secs(901);
    let expired_hex = hex_lower(&expired.vault_id);
    let expired_uri = format!("/admin/vaults/{expired_hex}");
    // EXPIRED 详情没有表单（不能取消），CSRF 从列表页取。
    let (_, _, home) = call(&app, get_req("/admin", Some(&client))).await;
    let csrf = csrf_from_page(&home);
    let (status, _, body) = call(
        &app,
        post_form_req(
            &format!("{expired_uri}/cancel"),
            &format!("csrf={csrf}"),
            Some(&client),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("EXPIRED"), "{body}");
    assert_eq!(
        env.state_of(expired.vault_id),
        Some(LifecycleState::Expired)
    );

    // 未知 id → 404。
    let (status, _, _) = call(
        &app,
        post_form_req(
            "/admin/vaults/00000000000000000000000000000000/cancel",
            &format!("csrf={csrf}"),
            Some(&client),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------- 5) 删除

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_ui_requires_exact_confirm_and_202_is_not_completion() {
    let env = block(|| WebEnv::active("s4a-delete", Some(DATA_PUBLIC_URL)));
    let app = env.app.clone();
    let client = login(&app).await;
    let vault = env.fixture_vault();
    let vault_hex = hex_lower(&vault);
    let detail_uri = format!("/admin/vaults/{vault_hex}");
    let delete_uri = format!("{detail_uri}/delete");

    // 详情页有删除入口；GET 删除页是确认表单（要求精确 `DELETE <id>`）。
    let (_, _, page) = call(&app, get_req(&detail_uri, Some(&client))).await;
    assert!(page.contains(&format!("href=\"{delete_uri}\"")), "{page}");
    let (status, _, page) = call(&app, get_req(&delete_uri, Some(&client))).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("确认删除"), "{page}");
    assert!(page.contains(&format!("DELETE {vault_hex}")), "{page}");
    assert!(page.contains("不可逆删除"), "{page}");
    let csrf = csrf_from_page(&page);

    // CSRF 缺失 → 403；确认字符串不严格 → 400，状态都不变。
    let (status, _, _) = call(
        &app,
        post_form_req(
            &delete_uri,
            &format!("confirm=DELETE+{vault_hex}"),
            Some(&client),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let mut bad_confirms = vec![
        vault_hex.clone(),              // 只有 id
        format!("DELETE++{vault_hex}"), // 两个空格
        format!("DELETE+{vault_hex}+"), // 尾随空格
        format!("delete+{vault_hex}"),  // 前缀大小写
    ];
    bad_confirms.push(format!("DELETE%20{vault_hex}%20"));
    for confirm in bad_confirms {
        let (status, _, body) = call(
            &app,
            post_form_req(
                &delete_uri,
                &format!("csrf={csrf}&confirm={confirm}"),
                Some(&client),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "confirm={confirm}: {body}");
        assert!(body.contains("确认不匹配"), "{body}");
        assert_eq!(env.state_of(vault), Some(LifecycleState::Active));
    }
    // 大小写严格：fixture id 恰好全数字，用一个含字母的 id 证明大写/小写不互通。
    let mixed = "0123456789abcdef0123456789abcdef";
    let (status, _, body) = call(
        &app,
        post_form_req(
            &format!("/admin/vaults/{mixed}/delete"),
            &format!("csrf={csrf}&confirm=DELETE+{}", mixed.to_uppercase()),
            Some(&client),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // 非 ACTIVE（RESERVED）→ 409，绝不把票据直接变墓碑。
    let reserved = block(|| env.reserved());
    let reserved_hex = hex_lower(&reserved.vault_id);
    let (_, _, page) = call(
        &app,
        get_req(
            &format!("/admin/vaults/{reserved_hex}/delete"),
            Some(&client),
        ),
    )
    .await;
    assert!(
        page.contains("只有 <code>ACTIVE</code> 能开始删除"),
        "{page}"
    );
    let (status, _, body) = call(
        &app,
        post_form_req(
            &format!("/admin/vaults/{reserved_hex}/delete"),
            &format!("csrf={csrf}&confirm=DELETE+{reserved_hex}"),
            Some(&client),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        env.state_of(reserved.vault_id),
        Some(LifecycleState::Reserved)
    );

    // 正确删除：停在第 0 批前，保证断言时仍是持久 DELETING。
    env.deletion
        .set_failpoint(Some(PurgeFailpoint::StopAfterBatches(0)));
    let mut events = env.deletion.subscribe();
    let (status, _, page) = call(
        &app,
        post_form_req(
            &delete_uri,
            &format!("csrf={csrf}&confirm=DELETE+{vault_hex}"),
            Some(&client),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{page}");
    assert!(
        page.contains("DELETING"),
        "202 后必须是持久 DELETING：{page}"
    );
    assert!(page.contains("<code>0/26</code>"), "{page}");
    assert!(!page.contains("已删除"), "202 页不得声称已完成：{page}");
    let row = env
        .db
        .read_instance(move |conn| registry::load_raw(conn, &vault))
        .expect("registry row")
        .expect("row");
    assert_eq!(row.state, LifecycleState::Deleting);
    let job_id = row.delete_job_id.expect("job id");

    // 详情页同样显示 DELETING 的进度/错误（生命周期小节）。
    let (_, _, detail_body) = call(&app, get_req(&detail_uri, Some(&client))).await;
    assert!(detail_body.contains("DELETING"), "{detail_body}");
    assert!(detail_body.contains("<code>0/26</code>"), "{detail_body}");

    // GET 进度页：DELETING + 进度 + meta refresh（无 JS）+ 明确"不代表已完成"。
    let (status, _, page) = call(&app, get_req(&delete_uri, Some(&client))).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("DELETING"), "{page}");
    assert!(page.contains("<code>0/26</code>"), "{page}");
    assert!(
        page.contains("http-equiv=\"refresh\""),
        "进度页必须有无 JS 的自动刷新：{page}"
    );
    assert!(page.contains("本页不代表已完成"), "{page}");
    assert!(page.contains(&hex_lower(&job_id)), "{page}");

    // 重复 POST → 202 同一 job（幂等），不影响进度页。
    let (status, _, page) = call(
        &app,
        post_form_req(
            &delete_uri,
            &format!("csrf={csrf}&confirm=DELETE+{vault_hex}"),
            Some(&client),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{page}");
    let row2 = env
        .db
        .read_instance(move |conn| registry::load_raw(conn, &vault))
        .expect("registry row")
        .expect("row");
    assert_eq!(row2.delete_job_id, Some(job_id), "重复删除必须复用同一 job");
    let (_, stopped) = drain_until_terminal(&mut events, vault).await;
    assert!(matches!(stopped, DeleteEvent::Stopped { .. }));

    // 收尾：让同一 job 跑完 → DELETED；列表/详情/进度页都显示墓碑，且无恢复按钮。
    env.deletion.shutdown().await;
    env.deletion.set_failpoint(None);
    env.deletion.start(vault).await.expect("resume delete");
    let (_, terminal) = drain_until_terminal(&mut events, vault).await;
    assert!(matches!(terminal, DeleteEvent::Deleted { .. }));

    let (_, _, list_body) = call(&app, get_req("/admin", Some(&client))).await;
    assert!(list_body.contains("DELETED"), "{list_body}");
    assert!(list_body.contains("<code>26/26</code>"), "{list_body}");
    let (_, _, detail_body) = call(&app, get_req(&detail_uri, Some(&client))).await;
    assert!(detail_body.contains("DELETED"), "{detail_body}");
    assert!(
        !detail_body.contains("删除该 vault"),
        "墓碑不得有删除入口：{detail_body}"
    );
    assert!(
        !detail_body.contains(">恢复") && !detail_body.contains("恢复</a>"),
        "不得提供恢复按钮：{detail_body}"
    );
    assert!(
        !detail_body.contains(">回滚") && !detail_body.contains("回滚</a>"),
        "不得提供回滚按钮：{detail_body}"
    );
    let (status, _, page) = call(&app, get_req(&delete_uri, Some(&client))).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("DELETED"), "{page}");
    assert!(page.contains("<code>26/26</code>"), "{page}");
    assert!(
        !page.contains("http-equiv=\"refresh\""),
        "终态不应自动刷新：{page}"
    );

    // 已删除后再 POST：仍是 202 幂等墓碑，不再有作业。
    let (status, _, page) = call(
        &app,
        post_form_req(
            &delete_uri,
            &format!("csrf={csrf}&confirm=DELETE+{vault_hex}"),
            Some(&client),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{page}");
    assert!(page.contains("DELETED"), "{page}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthenticated_actions_cannot_create_cancel_or_delete() {
    let env = block(|| WebEnv::active("s4a-authz", Some(DATA_PUBLIC_URL)));
    let app = env.app.clone();
    let vault = env.fixture_vault();
    let vault_hex = hex_lower(&vault);
    let reserved = block(|| env.reserved());
    let reserved_hex = hex_lower(&reserved.vault_id);

    // 所有写路由未登录都 303，且不改状态、不创建预留。
    for (uri, form) in [
        ("/admin/vaults".to_string(), "csrf=x".to_string()),
        (
            format!("/admin/vaults/{reserved_hex}/cancel"),
            "csrf=x".to_string(),
        ),
        (
            format!("/admin/vaults/{vault_hex}/delete"),
            format!("csrf=x&confirm=DELETE+{vault_hex}"),
        ),
    ] {
        let (status, headers, _) = call(&app, post_form_req(&uri, &form, None)).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{uri}");
        assert_eq!(location(&headers), Some("/admin/login"), "{uri}");
    }
    let reserved_count = env
        .list()
        .iter()
        .filter(|entry| entry.state == LifecycleState::Reserved)
        .count();
    assert_eq!(reserved_count, 1, "未登录 POST 不得创建新预留");
    assert_eq!(
        env.state_of(reserved.vault_id),
        Some(LifecycleState::Reserved)
    );
    assert_eq!(env.state_of(vault), Some(LifecycleState::Active));

    // 详情/进度页未登录也重定向（不暴露信息）。
    for uri in [
        format!("/admin/vaults/{vault_hex}"),
        format!("/admin/vaults/{vault_hex}/delete"),
    ] {
        let (status, headers, _) = call(&app, get_req(&uri, None)).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{uri}");
        assert_eq!(location(&headers), Some("/admin/login"));
    }
}
