//! 内嵌 Web 管理面（ADR 0018 S1）的 in-process HTTP 测试：真实 axum router + 临时 SQLite。
//!
//! 覆盖：登录/错误 token/限流/CSRF/Host/Origin、会话过期与登出、未初始化拒绝、
//! token 轮换吊销、admin cookie 与数据面隔离、body 上限、日志/响应不含 token。
//! 时间用 `FakeClock` 精确推进，**不 sleep**。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use tower::ServiceExt;

use menghunote_relay as relay;
use relay::admin_web::{
    self, AdminHostPolicy, AdminWeb, SESSION_ABSOLUTE_SECS, SESSION_COOKIE, SESSION_IDLE_SECS,
};
use relay::db::{Db, DbConfig};
use relay::deletion::Deletion;
use relay::time::FakeClock;

const BASE_TIME: i64 = 1_800_000_000;
const ADMIN_AUTHORITY: &str = "127.0.0.1:18081";
const ADMIN_ORIGIN: &str = "http://127.0.0.1:18081";

fn admin_addr() -> SocketAddr {
    ADMIN_AUTHORITY.parse().expect("admin authority")
}

fn temp_db_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "relay-admin-{}-{label}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir.join("relay.sqlite3")
}

/// 临时目录守卫：声明在结构体最后，保证 `Db` 关闭后才删除目录。
struct DbCleanup {
    dir: PathBuf,
}

impl Drop for DbCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 一个 admin 测试环境：真实 `Db` + `AdminWeb` + 真实 axum router。
struct AdminEnv {
    web: Arc<AdminWeb>,
    db: Db,
    clock: Arc<FakeClock>,
    token: Option<String>,
    config_conflict: bool,
    _cleanup: DbCleanup,
}

impl AdminEnv {
    /// 用 `token`（若给出）落库哈希，模拟 `init` 之后的实例。
    fn initialized(label: &str, token: &str) -> Self {
        Self::build(label, Some(token), None)
    }

    /// 未初始化：库里没有 admin token，也没有配置哈希。
    fn uninitialized(label: &str) -> Self {
        Self::build(label, None, None)
    }

    /// 库内有 token A、配置给 token B（验证库内优先）。
    fn with_config_conflict(label: &str, stored: &str, configured: &str) -> Self {
        Self::build(label, Some(stored), Some(configured))
    }

    /// 库内无 token、仅配置哈希（验证配置兜底）。
    fn configured_only(label: &str, configured: &str) -> Self {
        Self::build(label, None, Some(configured))
    }

    fn build(label: &str, token: Option<&str>, configured: Option<&str>) -> Self {
        let path = temp_db_path(label);
        let dir = path.parent().expect("parent").to_path_buf();
        let clock = Arc::new(FakeClock::new(BASE_TIME));
        let db = Db::open(DbConfig::new(path), clock.clone()).expect("open db");
        if let Some(token) = token {
            let hash = admin_web::token_hash(token);
            // `write_instance_blocking` 不能在 tokio runtime 线程上调用：放到独立线程里等它收敛。
            let writer_db = db.clone();
            std::thread::spawn(move || {
                writer_db
                    .write_instance_blocking(move |conn| admin_web::store_token_hash(conn, &hash))
            })
            .join()
            .expect("join db writer")
            .expect("store admin hash");
        }
        let configured_hash = configured.map(admin_web::token_hash);
        let resolved = admin_web::resolve_token_hash(&db, configured_hash).expect("resolve");
        let policy = AdminHostPolicy::for_listen(admin_addr(), &[]);
        let web = AdminWeb::new(
            db.clone(),
            clock.clone(),
            resolved.hash,
            None,
            policy,
            Arc::new(Deletion::new(db.clone())),
        );
        Self {
            web,
            db,
            clock,
            token: token.map(str::to_string),
            config_conflict: resolved.config_conflict,
            _cleanup: DbCleanup { dir },
        }
    }

    fn app(&self) -> Router {
        admin_web::router(self.web.clone())
    }

    fn token(&self) -> &str {
        self.token.as_deref().expect("token")
    }
}

// ---------------------------------------------------------------- 请求/响应辅助

fn get(uri: &str, host: Option<&str>, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(host) = host {
        builder = builder.header(header::HOST, host);
    }
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::empty()).expect("request")
}

fn post_form(
    uri: &str,
    body: &str,
    host: Option<&str>,
    origin: Option<&str>,
    referer: Option<&str>,
    cookie: Option<&str>,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(host) = host {
        builder = builder.header(header::HOST, host);
    }
    if let Some(origin) = origin {
        builder = builder.header(header::ORIGIN, origin);
    }
    if let Some(referer) = referer {
        builder = builder.header(header::REFERER, referer);
    }
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::from(body.to_string())).expect("request")
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

fn login_form(csrf: &str, token: &str) -> String {
    format!("csrf={csrf}&token={token}")
}

/// 打开登录页拿一次性 CSRF token。
async fn fresh_csrf(app: &Router) -> String {
    let (status, _, body) = call(app, get("/admin/login", Some(ADMIN_AUTHORITY), None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    csrf_from_page(&body)
}

/// 完整登录流程；返回状态码与（成功时的）会话 cookie。
async fn login(app: &Router, token: &str) -> (StatusCode, Option<String>) {
    let csrf = fresh_csrf(app).await;
    let (status, headers, _) = call(
        app,
        post_form(
            "/admin/login",
            &login_form(&csrf, token),
            Some(ADMIN_AUTHORITY),
            Some(ADMIN_ORIGIN),
            None,
            None,
        ),
    )
    .await;
    (status, session_cookie(&headers))
}

async fn login_ok(app: &Router, token: &str) -> String {
    let (status, cookie) = login(app, token).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "登录应成功");
    cookie.expect("session cookie")
}

fn meta_rows(db: &Db) -> Vec<(String, Vec<u8>)> {
    db.read_instance(|conn| {
        let mut stmt = conn.prepare("SELECT key, value FROM relay_meta")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
    .expect("read relay_meta")
}

// ---------------------------------------------------------------- 测试

#[tokio::test]
async fn login_page_has_security_headers_and_csrf_form() {
    let env = AdminEnv::initialized("page", "token-page");
    let app = env.app();

    let (status, headers, body) =
        call(&app, get("/admin/login", Some(ADMIN_AUTHORITY), None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("action=\"/admin/login\""), "{body}");
    assert!(body.contains("type=\"password\" name=\"token\""), "{body}");
    assert!(body.contains("明文 HTTP"), "登录页必须警告明文风险：{body}");
    assert!(!csrf_from_page(&body).is_empty());

    let header = |name: header::HeaderName| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(header(header::X_FRAME_OPTIONS), "DENY");
    assert!(
        header(header::CONTENT_SECURITY_POLICY).contains("default-src 'none'"),
        "缺少 CSP"
    );
    assert_eq!(header(header::CACHE_CONTROL), "no-store");
    assert_eq!(header(header::REFERRER_POLICY), "no-referrer");
    assert_eq!(header(header::X_CONTENT_TYPE_OPTIONS), "nosniff");
    assert!(
        headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none(),
        "不得启用默认 CORS"
    );
    // 未初始化提示只应在未初始化实例出现。
    assert!(!body.contains("尚未初始化"), "{body}");
}

#[tokio::test]
async fn forged_or_missing_host_is_rejected() {
    let env = AdminEnv::initialized("host", "token-host");
    let app = env.app();

    let (status, _, body) = call(
        &app,
        get("/admin/login", Some("evil.example.com:18081"), None),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("Host"), "{body}");

    let (status, _, body) = call(&app, get("/admin/login", None, None)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("Host"), "{body}");

    // 端口不同也不算命中（精确匹配）。
    let (status, _, _) = call(&app, get("/admin/login", Some("127.0.0.1:18082"), None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn login_success_sets_host_only_session_cookie_and_renders_home() {
    let env = AdminEnv::initialized("login-ok", "token-login-ok");
    let app = env.app();

    let csrf = fresh_csrf(&app).await;
    let (status, headers, _) = call(
        &app,
        post_form(
            "/admin/login",
            &login_form(&csrf, env.token()),
            Some(ADMIN_AUTHORITY),
            Some(ADMIN_ORIGIN),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers.get(header::LOCATION).and_then(|v| v.to_str().ok()),
        Some("/admin")
    );
    let raw_cookie = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with(SESSION_COOKIE))
        .expect("session cookie");
    assert!(raw_cookie.contains("HttpOnly"), "{raw_cookie}");
    assert!(raw_cookie.contains("SameSite=Strict"), "{raw_cookie}");
    assert!(raw_cookie.contains("Path=/"), "{raw_cookie}");
    assert!(
        !raw_cookie.contains("Domain="),
        "必须 host-only：{raw_cookie}"
    );
    assert!(
        !raw_cookie.contains("Secure"),
        "HTTP 登录不应带 Secure（文档警告明文风险）：{raw_cookie}"
    );
    assert!(
        !raw_cookie.contains(env.token()),
        "cookie 值不得是 admin token"
    );
    let cookie = session_cookie(&headers).expect("cookie pair");
    assert_eq!(env.web.sessions_len(), 1);

    let (status, _, body) = call(&app, get("/admin", Some(ADMIN_AUTHORITY), Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("已登录"), "{body}");
    assert!(body.contains("vault 数：<code>0</code>"), "{body}");
    assert!(body.contains("空库"), "{body}");
    assert!(body.contains("退出登录"), "{body}");
    assert!(!body.contains(env.token()), "页面不得回显 admin token");
    assert!(
        !body.contains("admin_token_sha256"),
        "页面不得展示 token 哈希"
    );

    // 无 cookie / 伪造 cookie → 跳转登录页。
    let (status, headers, _) = call(&app, get("/admin", Some(ADMIN_AUTHORITY), None)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers.get(header::LOCATION).and_then(|v| v.to_str().ok()),
        Some("/admin/login")
    );
    for bad in [
        "mn_admin_session=not-base64",
        "mn_admin_session=AAAA",
        "other=1",
    ] {
        let (status, _, _) = call(&app, get("/admin", Some(ADMIN_AUTHORITY), Some(bad))).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "伪造 cookie 必须拒绝：{bad}");
    }
}

#[tokio::test]
async fn wrong_token_is_unauthorized_and_never_echoed_or_stored() {
    let env = AdminEnv::initialized("wrong-token", "token-correct-value");
    let app = env.app();

    let csrf = fresh_csrf(&app).await;
    let wrong = "token-wrong-value";
    let (status, headers, body) = call(
        &app,
        post_form(
            "/admin/login",
            &login_form(&csrf, wrong),
            Some(ADMIN_AUTHORITY),
            Some(ADMIN_ORIGIN),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(session_cookie(&headers).is_none(), "失败不得下发会话");
    assert!(!body.contains(wrong), "错误页不得回显提交的 token：{body}");
    assert!(!body.contains(env.token()), "错误页不得回显真实 token");
    assert_eq!(env.web.sessions_len(), 0);

    // 库里只有 SHA256，且没有任何列出现明文。
    let rows = meta_rows(&env.db);
    let admin_hash = rows
        .iter()
        .find(|(key, _)| key == relay::db::schema::META_ADMIN_TOKEN_HASH)
        .map(|(_, value)| value.clone())
        .expect("admin hash row");
    assert_eq!(admin_hash, admin_web::token_hash(env.token()).to_vec());
    for (key, value) in &rows {
        assert!(
            value
                .windows(env.token().len())
                .all(|w| w != env.token().as_bytes()),
            "relay_meta[{key}] 不得出现 admin token 明文"
        );
    }
}

#[tokio::test]
async fn login_requires_csrf_and_same_origin() {
    let env = AdminEnv::initialized("csrf-origin", "token-csrf-origin");
    let app = env.app();

    // 缺 CSRF → 403（即使 Origin 正确）。
    let (status, _, body) = call(
        &app,
        post_form(
            "/admin/login",
            &format!("token={}", env.token()),
            Some(ADMIN_AUTHORITY),
            Some(ADMIN_ORIGIN),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("CSRF"), "{body}");

    // CSRF 一次性：同一个 token 第二次必须失败。
    let csrf = fresh_csrf(&app).await;
    let form = login_form(&csrf, env.token());
    let (status, _, _) = call(
        &app,
        post_form(
            "/admin/login",
            &form,
            Some(ADMIN_AUTHORITY),
            Some(ADMIN_ORIGIN),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let csrf = fresh_csrf(&app).await;
    let (status, _, _) = call(
        &app,
        post_form(
            "/admin/login",
            &login_form(&csrf, env.token()),
            Some(ADMIN_AUTHORITY),
            Some(ADMIN_ORIGIN),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // 跨站 Origin → 403。
    let csrf = fresh_csrf(&app).await;
    let (status, _, body) = call(
        &app,
        post_form(
            "/admin/login",
            &login_form(&csrf, env.token()),
            Some(ADMIN_AUTHORITY),
            Some("http://evil.example"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("跨站"), "{body}");

    // Origin 在 Host 白名单里但与 Host 不一致（不同主机名）→ 仍拒绝。
    let (status, _, _) = call(
        &app,
        post_form(
            "/admin/login",
            &login_form(&csrf, env.token()),
            Some(ADMIN_AUTHORITY),
            Some("http://localhost:18081"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "Origin 必须与 Host 完全一致");

    // Origin: null / 无 Origin、无 Referer：来源不透明，交给 CSRF；一次性 token 有效时可登录。
    for origin in [Some("null"), None] {
        let csrf = fresh_csrf(&app).await;
        let (status, _, _) = call(
            &app,
            post_form(
                "/admin/login",
                &login_form(&csrf, env.token()),
                Some(ADMIN_AUTHORITY),
                origin,
                None,
                None,
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "不透明/缺失来源应靠 CSRF 放行: {origin:?}"
        );
    }

    // 不透明来源但 CSRF 无效仍拒绝。
    let (status, _, _) = call(
        &app,
        post_form(
            "/admin/login",
            &format!("csrf=bad&token={}", env.token()),
            Some(ADMIN_AUTHORITY),
            Some("null"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Referer 回退：同源 Referer 可以登录。
    let csrf = fresh_csrf(&app).await;
    let (status, headers, _) = call(
        &app,
        post_form(
            "/admin/login",
            &login_form(&csrf, env.token()),
            Some(ADMIN_AUTHORITY),
            None,
            Some("http://127.0.0.1:18081/admin/login"),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(session_cookie(&headers).is_some());
}

#[tokio::test]
async fn failed_logins_are_rate_limited() {
    let env = AdminEnv::initialized("rate", "token-rate");
    let app = env.app();

    for attempt in 1..=5u8 {
        let csrf = fresh_csrf(&app).await;
        let (status, _, _) = call(
            &app,
            post_form(
                "/admin/login",
                &login_form(&csrf, &format!("wrong-{attempt}")),
                Some(ADMIN_AUTHORITY),
                Some(ADMIN_ORIGIN),
                None,
                None,
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "第 {attempt} 次失败应为 401"
        );
    }
    // 第 6 次：即使 token 正确也被限流（按 IP 失败窗口）。
    let csrf = fresh_csrf(&app).await;
    let (status, headers, body) = call(
        &app,
        post_form(
            "/admin/login",
            &login_form(&csrf, env.token()),
            Some(ADMIN_AUTHORITY),
            Some(ADMIN_ORIGIN),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(session_cookie(&headers).is_none());
    assert!(body.contains("限流"), "{body}");
}

#[tokio::test]
async fn idle_expiry_logout_and_token_rotation_revoke_sessions() {
    let env = AdminEnv::initialized("session-life", "token-session-life");
    let app = env.app();

    // 闲置过期：30 分钟无活动即失效；活动会续期。
    let cookie = login_ok(&app, env.token()).await;
    env.clock.advance_secs(SESSION_IDLE_SECS - 1);
    let (status, _, _) = call(&app, get("/admin", Some(ADMIN_AUTHORITY), Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    env.clock.advance_secs(SESSION_IDLE_SECS);
    let (status, headers, _) =
        call(&app, get("/admin", Some(ADMIN_AUTHORITY), Some(&cookie))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers.get(header::LOCATION).and_then(|v| v.to_str().ok()),
        Some("/admin/login")
    );
    assert_eq!(env.web.sessions_len(), 0, "过期条目应被清理");

    // 登出：需要会话 CSRF；成功后旧 cookie 立即失效。
    let cookie = login_ok(&app, env.token()).await;
    let (_, _, page) = call(&app, get("/admin", Some(ADMIN_AUTHORITY), Some(&cookie))).await;
    let csrf = csrf_from_page(&page);
    let (status, headers, _) = call(
        &app,
        post_form(
            "/admin/logout",
            &format!("csrf={csrf}"),
            Some(ADMIN_AUTHORITY),
            Some(ADMIN_ORIGIN),
            None,
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let cleared = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with(SESSION_COOKIE))
        .expect("clear cookie");
    assert!(cleared.contains("Max-Age=0"), "{cleared}");
    assert_eq!(env.web.sessions_len(), 0);
    let (status, _, _) = call(&app, get("/admin", Some(ADMIN_AUTHORITY), Some(&cookie))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // 登出缺 CSRF → 403，会话保留。
    let cookie = login_ok(&app, env.token()).await;
    let (status, _, _) = call(
        &app,
        post_form(
            "/admin/logout",
            "",
            Some(ADMIN_AUTHORITY),
            Some(ADMIN_ORIGIN),
            None,
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(env.web.sessions_len(), 1);

    // 换 token：旧会话立即失效；旧 token 登录失败；新 token 可登录。
    let new_token = admin_web::generate_token().expect("token");
    env.web
        .set_token_hash(Some(admin_web::token_hash(&new_token)));
    assert_eq!(env.web.sessions_len(), 0, "换 token 必须清空会话");
    let (status, _, _) = call(&app, get("/admin", Some(ADMIN_AUTHORITY), Some(&cookie))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _) = login(&app, env.token()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let cookie = login_ok(&app, &new_token).await;
    let (status, _, _) = call(&app, get("/admin", Some(ADMIN_AUTHORITY), Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn absolute_expiry_is_not_extended_by_activity() {
    let env = AdminEnv::initialized("absolute", "token-absolute");
    let app = env.app();
    let cookie = login_ok(&app, env.token()).await;

    let step = SESSION_IDLE_SECS * 2 / 3; // 20 分钟：小于闲置窗口，保证只有绝对过期生效。
    let mut alive = true;
    for _ in 0..(SESSION_ABSOLUTE_SECS / step) {
        env.clock.advance_secs(step);
        let (status, _, _) = call(&app, get("/admin", Some(ADMIN_AUTHORITY), Some(&cookie))).await;
        alive = status == StatusCode::OK;
        if !alive {
            break;
        }
    }
    assert!(!alive, "绝对过期（8 小时）必须生效，活动不能续命");
}

// ---------------------------------------------------------------- 未初始化 / 隔离 / 上限

fn vault_b64() -> String {
    relay::ids::b64url_encode(&[0x11u8; 16])
}

fn data_app(env: &AdminEnv) -> Router {
    let config = Arc::new(relay::config::RelayConfig::new(env.db.path().to_path_buf()));
    relay::api::router(relay::api::AppState::new(
        env.db.clone(),
        config,
        Arc::new(relay::verify::Ed25519Hook),
    ))
}

#[tokio::test]
async fn uninitialized_admin_rejects_login_and_never_generates_token() {
    let env = AdminEnv::uninitialized("uninitialized");
    let app = env.app();

    let (status, _, body) = call(&app, get("/admin/login", Some(ADMIN_AUTHORITY), None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("尚未初始化"), "登录页必须提示先 init：{body}");
    let csrf = csrf_from_page(&body);
    let (status, headers, body) = call(
        &app,
        post_form(
            "/admin/login",
            &login_form(&csrf, "any-token"),
            Some(ADMIN_AUTHORITY),
            Some(ADMIN_ORIGIN),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("init"), "{body}");
    assert!(session_cookie(&headers).is_none());
    assert_eq!(env.web.sessions_len(), 0);

    // **绝不自动生成**：库里仍然没有 admin token 哈希。
    assert!(meta_rows(&env.db)
        .iter()
        .all(|(key, _)| key != relay::db::schema::META_ADMIN_TOKEN_HASH));

    // 数据面正常（未鉴权返回协议 401，而不是 500/崩溃）。
    let data = data_app(&env);
    let (status, headers, _) = call(
        &data,
        get(
            &format!("/v1/vaults/{}/state", vault_b64()),
            Some(ADMIN_AUTHORITY),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(headers.get(header::SET_COOKIE).is_none());
}

#[tokio::test]
async fn admin_cookie_is_not_accepted_by_data_plane() {
    let env = AdminEnv::initialized("isolation", "token-isolation");
    let admin = env.app();
    let cookie = login_ok(&admin, env.token()).await;
    let data = data_app(&env);

    // 数据面不认 admin session cookie（从不读 cookie），且不下发任何 cookie。
    let (status, headers, _) = call(
        &data,
        get(
            &format!("/v1/vaults/{}/state", vault_b64()),
            Some(ADMIN_AUTHORITY),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        headers.get(header::SET_COOKIE).is_none(),
        "数据面不得返回 admin session"
    );

    // 数据 listener 不挂 admin 路由；admin listener 也不挂数据面路由。
    let (status, _, _) = call(
        &data,
        get("/admin/login", Some(ADMIN_AUTHORITY), Some(&cookie)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = call(
        &admin,
        get(
            &format!("/v1/vaults/{}/state", vault_b64()),
            Some(ADMIN_AUTHORITY),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // 管理页也不接受数据面 bearer（无 Bearer 头，只认 admin cookie）。
    let request = Request::builder()
        .method("GET")
        .uri("/admin")
        .header(header::HOST, ADMIN_AUTHORITY)
        .header(header::AUTHORIZATION, "Bearer not-an-admin-session")
        .body(Body::empty())
        .expect("request");
    let (status, headers, _) = call(&admin, request).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers.get(header::LOCATION).and_then(|v| v.to_str().ok()),
        Some("/admin/login")
    );
}

#[tokio::test]
async fn oversized_login_body_is_rejected() {
    let env = AdminEnv::initialized("body-limit", "token-body-limit");
    let app = env.app();
    let csrf = fresh_csrf(&app).await;
    let huge = format!(
        "csrf={csrf}&token={}",
        "a".repeat(admin_web::ADMIN_BODY_LIMIT)
    );
    assert!(huge.len() > admin_web::ADMIN_BODY_LIMIT);
    let (status, _, _) = call(
        &app,
        post_form(
            "/admin/login",
            &huge,
            Some(ADMIN_AUTHORITY),
            Some(ADMIN_ORIGIN),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn database_hash_wins_over_configured_hash() {
    let stored = admin_web::generate_token().expect("token");
    let configured = admin_web::generate_token().expect("token");
    let env = AdminEnv::with_config_conflict("db-priority", &stored, &configured);
    assert!(env.config_conflict, "两处哈希不一致必须可被注意到了");
    let app = env.app();

    let (status, _) = login(&app, &configured).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "配置哈希不得覆盖库内值");
    let cookie = login_ok(&app, &stored).await;
    let (status, _, _) = call(&app, get("/admin", Some(ADMIN_AUTHORITY), Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn configured_hash_is_used_when_database_has_none() {
    let configured = admin_web::generate_token().expect("token");
    let env = AdminEnv::configured_only("config-fallback", &configured);
    assert!(!env.config_conflict);
    assert!(env.web.has_token());
    let app = env.app();
    let cookie = login_ok(&app, &configured).await;
    let (status, _, _) = call(&app, get("/admin", Some(ADMIN_AUTHORITY), Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    // 登录/访问不写库：库内仍然没有 admin 哈希。
    assert!(meta_rows(&env.db)
        .iter()
        .all(|(key, _)| key != relay::db::schema::META_ADMIN_TOKEN_HASH));
}

#[tokio::test]
async fn https_origin_sets_secure_cookie_without_trusting_proxy_headers() {
    let env = AdminEnv::initialized("secure-cookie", "token-secure-cookie");
    let app = env.app();
    let csrf = fresh_csrf(&app).await;
    // 经 TLS 反向代理时浏览器 Origin 是 https；relay 自身只讲明文，但应把 cookie 标 Secure。
    let (status, headers, _) = call(
        &app,
        post_form(
            "/admin/login",
            &login_form(&csrf, env.token()),
            Some(ADMIN_AUTHORITY),
            Some("https://127.0.0.1:18081"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let raw_cookie = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with(SESSION_COOKIE))
        .expect("session cookie");
    assert!(raw_cookie.contains("; Secure"), "{raw_cookie}");
    assert!(!raw_cookie.contains("Domain="), "{raw_cookie}");

    // 仅 `X-Forwarded-Proto: https` 不被信任：HTTP Origin 下 cookie 不带 Secure。
    let csrf = fresh_csrf(&app).await;
    let mut request = post_form(
        "/admin/login",
        &login_form(&csrf, env.token()),
        Some(ADMIN_AUTHORITY),
        Some(ADMIN_ORIGIN),
        None,
        None,
    );
    request.headers_mut().insert(
        "x-forwarded-proto",
        axum::http::HeaderValue::from_static("https"),
    );
    let (status, headers, _) = call(&app, request).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let raw_cookie = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with(SESSION_COOKIE))
        .expect("session cookie");
    assert!(
        !raw_cookie.contains("; Secure"),
        "不得信任可伪造的 X-Forwarded-Proto：{raw_cookie}"
    );
}
