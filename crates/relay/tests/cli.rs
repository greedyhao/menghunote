//! 生产 CLI 的进程级测试：真跑 `menghunote-relay` 二进制（不绑生产端口、不联网）。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use menghunote_relay as relay;

const BIN: &str = env!("CARGO_BIN_EXE_menghunote-relay");

fn tmp_path(label: &str) -> PathBuf {
    let base = option_env!("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let dir = base.join(format!("relay-cli-{}-{label}-{nanos}", std::process::id()));
    if dir.exists() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    std::fs::create_dir_all(&dir).expect("create tmp dir");
    dir.join("relay.sqlite3")
}

fn cleanup(db: &Path) {
    if let Some(dir) = db.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

fn toml_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn run(args: &[&str], stdin: Option<&str>) -> Output {
    let mut command = Command::new(BIN);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().expect("spawn relay binary");
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write stdin");
    }
    child.wait_with_output().expect("wait relay binary")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn init_then_status_reports_uninitialized_vault_and_reinit_is_rejected() {
    let db = tmp_path("init-status");
    let db_arg = db.to_string_lossy().into_owned();

    let output = run(&["init", "--db", &db_arg], None);
    assert!(output.status.success(), "init failed: {}", stderr(&output));
    let printed = stdout(&output);
    assert!(printed.contains("setup_secret="), "{printed}");
    assert!(printed.contains("instance_id="), "{printed}");
    assert!(!printed.contains("setup_secret=\n"), "secret 不能为空");

    let output = run(&["status", "--db", &db_arg], None);
    assert!(
        output.status.success(),
        "status failed: {}",
        stderr(&output)
    );
    let printed = stdout(&output);
    assert!(printed.contains("state=uninitialized"), "{printed}");
    assert!(printed.contains("vaults=0"), "{printed}");
    assert!(
        printed.contains(&format!(
            "schema_version={}",
            menghunote_relay::db::schema::SCHEMA_VERSION
        )),
        "{printed}"
    );
    assert!(printed.contains("instance_id="), "{printed}");
    assert!(!printed.contains("setup_secret"), "status 不得回显 secret");

    // 第二次 init 必须拒绝（setup secret 只生成一次）。
    let output = run(&["init", "--db", &db_arg], None);
    assert!(!output.status.success(), "second init must fail");
    assert!(stderr(&output).contains("init"), "{}", stderr(&output));

    cleanup(&db);
}

#[test]
fn status_is_read_only_existing_only_and_rejects_incompatible_schema() {
    let db = tmp_path("status-readonly");
    let db_arg = db.to_string_lossy().into_owned();

    // 不存在的库：status 必须失败，且**不得创建**库文件。
    let output = run(&["status", "--db", &db_arg], None);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("数据库不存在") && stderr(&output).contains("不会建库"),
        "{}",
        stderr(&output)
    );
    assert!(!db.exists(), "status 不得创建数据库文件");

    // 初始化后 status 正常（只读连接读 schema 13）。
    let output = run(&["init", "--db", &db_arg], None);
    assert!(output.status.success(), "{}", stderr(&output));
    let output = run(&["status", "--db", &db_arg], None);
    assert!(output.status.success(), "{}", stderr(&output));

    // 把元数据改成旧版本：status 必须明确拒绝、**不迁移、不写库**。
    {
        let conn = rusqlite::Connection::open(&db).expect("open");
        conn.execute(
            "UPDATE relay_meta SET value = x'3132' WHERE key = 'schema_version'",
            [],
        )
        .expect("tamper schema_version");
    }
    let before = std::fs::read(&db).expect("read db before");
    let output = run(&["status", "--db", &db_arg], None);
    assert!(!output.status.success(), "schema 不兼容必须失败");
    let message = stderr(&output);
    assert!(
        message.contains("schema_version=12")
            && message.contains("不会自动迁移")
            && message.contains("只读"),
        "{message}"
    );
    assert_eq!(
        std::fs::read(&db).expect("read db after"),
        before,
        "status 不得写库/迁移"
    );
    cleanup(&db);
}

#[test]
fn serve_rejects_invalid_config_before_binding() {
    let dir = tmp_path("cfg").parent().expect("parent").to_path_buf();
    let write_config = |name: &str, body: &str| -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write config");
        path
    };

    // 非法 listen。
    let config = write_config(
        "bad-listen.toml",
        "listen = \"not-an-address\"\ndb = \"C:/tmp/x.sqlite3\"\n",
    );
    let output = run(&["serve", "--config", &config.to_string_lossy()], None);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("listen"), "{}", stderr(&output));

    // 非法 public_url（ADR 0018 S4a）：必须是 canonical origin，且明文只允许 loopback。
    let config = write_config(
        "bad-public-url-path.toml",
        &format!(
            "listen = \"127.0.0.1:18080\"\ndb = \"{}\"\npublic_url = \"http://127.0.0.1:18080/api\"\n",
            toml_path(&dir.join("x.sqlite3"))
        ),
    );
    let output = run(&["serve", "--config", &config.to_string_lossy()], None);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("public_url") && stderr(&output).contains("canonical"),
        "{}",
        stderr(&output)
    );
    let config = write_config(
        "bad-public-url-lan.toml",
        &format!(
            "listen = \"127.0.0.1:18080\"\ndb = \"{}\"\npublic_url = \"http://192.168.1.10:18080\"\n",
            toml_path(&dir.join("x.sqlite3"))
        ),
    );
    let output = run(&["serve", "--config", &config.to_string_lossy()], None);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("public_url") && stderr(&output).contains("loopback"),
        "{}",
        stderr(&output)
    );

    // 未知字段（例如试图开启放行 hook）→ 拒绝。
    let config = write_config(
        "permissive.toml",
        "listen = \"127.0.0.1:18080\"\ndb = \"C:/tmp/x.sqlite3\"\npermissive_hook = true\n",
    );
    let output = run(&["serve", "--config", &config.to_string_lossy()], None);
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(
        message.contains("permissive_hook") || message.contains("unknown field"),
        "{message}"
    );
    cleanup(&dir.join("unused.sqlite3"));
}

#[test]
fn serve_refuses_uninitialized_database() {
    let db = tmp_path("uninitialized");
    let dir = db.parent().expect("parent").to_path_buf();
    let config = dir.join("valid.toml");
    let body = format!("listen = \"127.0.0.1:0\"\ndb = \"{}\"\n", toml_path(&db));
    std::fs::write(&config, body).expect("write config");
    let output = run(&["serve", "--config", &config.to_string_lossy()], None);
    assert!(!output.status.success(), "未 init 的库必须拒绝启动");
    assert!(
        stderr(&output).contains("尚未初始化"),
        "{}",
        stderr(&output)
    );
    assert!(db.exists(), "拒绝启动前已建库（会话内可继续 init）");
    cleanup(&db);
}

#[test]
fn serve_rejects_non_loopback_plaintext_without_explicit_allow() {
    let db = tmp_path("non-loopback-plaintext");
    let dir = db.parent().expect("parent").to_path_buf();
    let config = dir.join("plaintext.toml");
    let body = format!("listen = \"0.0.0.0:0\"\ndb = \"{}\"\n", toml_path(&db));
    std::fs::write(&config, body).expect("write config");

    let output = run(&["serve", "--config", &config.to_string_lossy()], None);
    assert!(!output.status.success(), "非 loopback 明文必须拒绝启动");
    let message = stderr(&output);
    assert!(message.contains("allow_plaintext"), "{message}");
    assert!(
        message.contains("反向代理"),
        "要给出 TLS 部署做法：{message}"
    );
    assert!(
        !db.exists(),
        "明文门禁必须在打开/创建数据库之前拒绝（不能留半初始化文件）"
    );
    cleanup(&db);
}

/// 子进程守卫：任何 panic 路径都先杀掉 relay，避免测试失败后留下孤儿进程
/// （孤儿会持有端口/库文件，还可能拖住 CI 的进程树等待）。
struct ChildGuard {
    child: Child,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn serve_non_loopback_plaintext_starts_after_explicit_allow() {
    let db = tmp_path("allow-plaintext");
    let db_arg = db.to_string_lossy().into_owned();
    let output = run(&["init", "--db", &db_arg], None);
    assert!(output.status.success(), "{}", stderr(&output));

    let dir = db.parent().expect("parent").to_path_buf();
    let config = dir.join("serve.toml");
    let body = format!(
        "listen = \"0.0.0.0:0\"\ndb = \"{}\"\nallow_plaintext = true\n",
        toml_path(&db)
    );
    std::fs::write(&config, body).expect("write config");

    let config_arg = config.to_string_lossy().into_owned();
    let child = Command::new(BIN)
        .args(["serve", "--config", config_arg.as_str()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn relay");
    let mut guard = ChildGuard { child };
    let mut stdout = BufReader::new(guard.child.stdout.take().expect("stdout"));

    // 等启动横幅（blocking read_line 是明确收敛点，不是 sleep）。
    let mut port = None;
    let mut banner = String::new();
    for _ in 0..20 {
        let mut line = String::new();
        if stdout.read_line(&mut line).expect("read stdout") == 0 {
            break;
        }
        banner.push_str(&line);
        if let Some(rest) = line.split("listening on http://").nth(1) {
            let authority = rest
                .split(|c: char| c.is_whitespace() || c == '（')
                .next()
                .unwrap_or("");
            port = authority
                .rsplit_once(':')
                .and_then(|(_, port)| port.parse::<u16>().ok());
            if port.is_some() {
                break;
            }
        }
    }
    let Some(port) = port else {
        panic!("启动后没有打印 listening 横幅：{banner}");
    };

    // 显式允许后确实能接受连接并讲 HTTP（服务端会回 401/404 之类的 CBOR 错误体，
    // 只断言状态行；body 可能不是 UTF-8，必须按字节读）。
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect allowed server");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    stream
        .write_all(
            b"GET /v1/vaults/AAAA/state HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        )
        .expect("write request");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    drop(stdout);
    drop(guard);
    cleanup(&db);

    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 "),
        "显式 allow_plaintext 后必须能讲 HTTP：{response}"
    );
}

// ---------------------------------------------------------------- ADR 0018 S1：admin Web

#[test]
fn init_prints_admin_token_exactly_once() {
    let db = tmp_path("init-admin-token");
    let db_arg = db.to_string_lossy().into_owned();
    let output = run(&["init", "--db", &db_arg], None);
    assert!(output.status.success(), "init failed: {}", stderr(&output));
    let printed = stdout(&output);

    let admin_lines: Vec<&str> = printed
        .lines()
        .filter_map(|line| line.strip_prefix("admin_token="))
        .collect();
    assert_eq!(admin_lines.len(), 1, "admin token 只打印一次：{printed}");
    let admin_token = admin_lines[0];
    assert_eq!(admin_token.len(), 43, "32B → base64url 43 字符");
    assert_ne!(
        Some(admin_token),
        printed
            .lines()
            .find_map(|line| line.strip_prefix("setup_secret=")),
        "admin token 必须与 setup secret 分离"
    );
    let printed_hash = printed
        .lines()
        .find_map(|line| line.strip_prefix("admin_token_sha256="))
        .expect("admin_token_sha256 行");
    assert_eq!(
        printed_hash,
        relay::ids::hex_lower(&relay::ids::sha256(admin_token.as_bytes())),
        "打印的哈希必须是 SHA256(admin token 文本)"
    );
    assert!(
        printed.contains("只显示这一次"),
        "必须提示只显示一次：{printed}"
    );
    assert!(!printed.contains("admin_token=\n"), "admin token 不能为空");

    // 二次 init 拒绝，且不再打印新 token。
    let output = run(&["init", "--db", &db_arg], None);
    assert!(!output.status.success());
    assert!(
        !stdout(&output).contains("admin_token="),
        "二次 init 不得重新生成/打印 admin token"
    );
    cleanup(&db);
}

/// 选一个本机空闲端口（bind 后立即释放；测试内没有其他绑定者）。
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    listener.local_addr().expect("local addr").port()
}

/// 一次选两个**互不相同**的空闲端口（两个 listener 同时 bind 再释放，避免内核复用同一端口）。
fn free_ports() -> (u16, u16) {
    let first = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let second = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let a = first.local_addr().expect("addr").port();
    let b = second.local_addr().expect("addr").port();
    assert_ne!(a, b, "内核必须分配两个不同端口");
    (a, b)
}

fn write_serve_config(db: &Path, data_port: u16, admin: &str) -> PathBuf {
    let dir = db.parent().expect("parent").to_path_buf();
    let config = dir.join("serve.toml");
    let body = format!(
        "listen = \"127.0.0.1:{data_port}\"\ndb = \"{}\"\nadmin_listen = \"{admin}\"\npublic_url = \"http://127.0.0.1:{data_port}\"\n",
        toml_path(db)
    );
    std::fs::write(&config, body).expect("write config");
    config
}

fn spawn_serve(config: &Path) -> ChildGuard {
    let child = Command::new(BIN)
        .args(["serve", "--config", &config.to_string_lossy()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn relay");
    ChildGuard { child }
}

/// 从启动横幅行里取 `http://host:port` 的端口。
fn port_from_banner(line: &str) -> Option<u16> {
    let rest = line.split("http://").nth(1)?;
    let authority = rest
        .split(|c: char| c.is_whitespace() || c == '（')
        .next()
        .unwrap_or("");
    authority
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
}

/// 读到数据面 + admin 两条横幅（blocking read_line 是明确收敛点，不是 sleep）。
fn read_dual_banners(stdout: &mut impl BufRead) -> (u16, u16, String) {
    let mut data_port = None;
    let mut admin_port = None;
    let mut seen = String::new();
    for _ in 0..40 {
        let mut line = String::new();
        if stdout.read_line(&mut line).expect("read stdout") == 0 {
            break;
        }
        seen.push_str(&line);
        if line.contains("[relay] admin listening on http://") {
            admin_port = admin_port.or_else(|| port_from_banner(&line));
        } else if line.contains("[relay] listening on http://") {
            data_port = data_port.or_else(|| port_from_banner(&line));
        }
        if data_port.is_some() && admin_port.is_some() {
            break;
        }
    }
    match (data_port, admin_port) {
        (Some(data), Some(admin)) => (data, admin, seen),
        _ => panic!("没有读到两个 listener 的横幅：{seen}"),
    }
}

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

fn http_request(port: u16, raw: &str) -> HttpResponse {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect relay");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    stream.write_all(raw.as_bytes()).expect("write request");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    let text = String::from_utf8_lossy(&response);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|line| {
            line.split_once(':')
                .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    HttpResponse {
        status,
        headers,
        body: body.to_string(),
    }
}

fn admin_get(port: u16, path: &str, authority: &str, cookie: Option<&str>) -> HttpResponse {
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n");
    if let Some(cookie) = cookie {
        request.push_str(&format!("Cookie: {cookie}\r\n"));
    }
    request.push_str("\r\n");
    http_request(port, &request)
}

fn admin_form_post(port: u16, path: &str, authority: &str, form: &str) -> HttpResponse {
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nOrigin: http://{authority}\r\n\
         Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{form}",
        form.len()
    );
    http_request(port, &request)
}

fn csrf_from_html(body: &str) -> String {
    let marker = "name=\"csrf\" value=\"";
    let start = body
        .find(marker)
        .unwrap_or_else(|| panic!("页面里没有 CSRF token：{body}"))
        + marker.len();
    let rest = &body[start..];
    rest[..rest.find('"').expect("csrf end")].to_string()
}

/// 杀掉 relay 并收集 stderr；stdout 可能已被调用方拿走读横幅，故由调用方自行 drain。
fn stop(mut guard: ChildGuard) -> String {
    let mut stderr_pipe = guard.child.stderr.take().expect("stderr");
    let _ = guard.child.kill();
    let _ = guard.child.wait();
    let mut stderr_text = String::new();
    let _ = stderr_pipe.read_to_string(&mut stderr_text);
    stderr_text
}

#[test]
fn serve_starts_both_listeners_and_admin_login_works() {
    let db = tmp_path("serve-admin-e2e");
    let db_arg = db.to_string_lossy().into_owned();
    let output = run(&["init", "--db", &db_arg], None);
    assert!(output.status.success(), "{}", stderr(&output));
    let printed = stdout(&output);
    let admin_token = printed
        .lines()
        .find_map(|line| line.strip_prefix("admin_token="))
        .expect("admin token")
        .to_string();

    let (chosen_data_port, chosen_admin_port) = free_ports();
    let config = write_serve_config(
        &db,
        chosen_data_port,
        &format!("127.0.0.1:{chosen_admin_port}"),
    );
    let mut guard = spawn_serve(&config);
    let mut stdout = BufReader::new(guard.child.stdout.take().expect("stdout"));
    let (data_port, admin_port, banners) = read_dual_banners(&mut stdout);
    let admin_listen = format!("127.0.0.1:{admin_port}");
    assert!(
        banners.contains(&format!(
            "[relay] listening on http://127.0.0.1:{data_port}"
        )),
        "{banners}"
    );
    assert!(
        banners.contains("admin token") || banners.contains("admin listening"),
        "{banners}"
    );
    assert!(
        banners.contains(&format!("[relay] public_url=http://127.0.0.1:{data_port}")),
        "启动必须打印生效的 public_url（只来自配置）：{banners}"
    );

    // 1) admin 登录页：Host 精确匹配；form 带 CSRF。
    let page = admin_get(admin_port, "/admin/login", &admin_listen, None);
    assert_eq!(page.status, 200, "{:?}", page.body);
    assert_eq!(page.header("x-frame-options"), Some("DENY"));
    assert_eq!(page.header("cache-control"), Some("no-store"));
    let csrf = csrf_from_html(&page.body);

    // 2) token 登录：303 + host-only 会话 cookie。
    let response = admin_form_post(
        admin_port,
        "/admin/login",
        &admin_listen,
        &format!("csrf={csrf}&token={admin_token}"),
    );
    assert_eq!(response.status, 303, "{}", response.body);
    assert_eq!(response.header("location"), Some("/admin"));
    let set_cookie = response
        .header("set-cookie")
        .expect("set-cookie")
        .to_string();
    assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
    assert!(set_cookie.contains("SameSite=Strict"), "{set_cookie}");
    assert!(!set_cookie.contains("Secure"), "{set_cookie}");
    let cookie = set_cookie
        .split(';')
        .next()
        .expect("cookie pair")
        .to_string();

    // 3) 登录后首页：显示已登录/空库。
    let home = admin_get(admin_port, "/admin", &admin_listen, Some(&cookie));
    assert_eq!(home.status, 200, "{}", home.body);
    assert!(home.body.contains("已登录"), "{}", home.body);
    assert!(home.body.contains("空库"), "{}", home.body);
    assert!(
        home.body.contains(&format!("http://127.0.0.1:{data_port}")),
        "管理页必须只读显示配置的数据面 URL：{}",
        home.body
    );
    assert!(
        !home.body.contains(&admin_token),
        "页面不得回显 admin token"
    );

    // 4) admin/数据 listener 路由完全隔离。
    let admin_on_data = admin_get(data_port, "/admin/login", &admin_listen, None);
    assert_eq!(admin_on_data.status, 404, "数据面不挂 admin 路由");
    let vault = relay::ids::b64url_encode(&[0x22u8; 16]);
    let data_on_admin = admin_get(
        admin_port,
        &format!("/v1/vaults/{vault}/state"),
        &admin_listen,
        Some(&cookie),
    );
    assert_eq!(data_on_admin.status, 404, "admin 面不挂数据路由");

    // 5) 数据面带 admin cookie 仍 401，且不回写 cookie。
    let data_with_cookie = admin_get(
        data_port,
        &format!("/v1/vaults/{vault}/state"),
        &format!("127.0.0.1:{data_port}"),
        Some(&cookie),
    );
    assert_eq!(data_with_cookie.status, 401);
    assert!(data_with_cookie.header("set-cookie").is_none());

    // 6) 伪造 Host / 跨站 Origin 都被拒绝。
    let forged_host = admin_get(admin_port, "/admin/login", "evil.example.com", None);
    assert_eq!(forged_host.status, 403, "{}", forged_host.body);
    let csrf = csrf_from_html(&admin_get(admin_port, "/admin/login", &admin_listen, None).body);
    let cross_site = {
        let request = format!(
            "POST /admin/login HTTP/1.1\r\nHost: {admin_listen}\r\nOrigin: http://evil.example\r\n\
             Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\ncsrf={csrf}&token={admin_token}",
            format!("csrf={csrf}&token={admin_token}").len()
        );
        http_request(admin_port, &request)
    };
    assert_eq!(cross_site.status, 403, "{}", cross_site.body);

    let serve_stderr = stop(guard);
    let mut serve_stdout = banners.clone();
    let _ = stdout.read_to_string(&mut serve_stdout);
    assert!(
        !serve_stdout.contains(&admin_token),
        "serve 日志不得打印 admin token：{serve_stdout}"
    );
    assert!(
        !serve_stderr.contains(&admin_token),
        "serve stderr 不得打印 admin token：{serve_stderr}"
    );
    cleanup(&db);
}

#[test]
fn serve_fails_fast_when_admin_port_is_occupied() {
    let db = tmp_path("admin-port-conflict");
    let db_arg = db.to_string_lossy().into_owned();
    let output = run(&["init", "--db", &db_arg], None);
    assert!(output.status.success(), "{}", stderr(&output));

    // 占住 admin 端口（数据面端口保持空闲）。
    let occupied = TcpListener::bind("127.0.0.1:0").expect("occupy");
    let admin_port = occupied.local_addr().expect("addr").port();
    let data_port = free_port();
    let config = write_serve_config(&db, data_port, &format!("127.0.0.1:{admin_port}"));

    let output = Command::new(BIN)
        .args(["serve", "--config", &config.to_string_lossy()])
        .output()
        .expect("run relay");
    let printed = stdout(&output);
    let message = stderr(&output);
    assert!(!output.status.success(), "admin 端口冲突必须启动失败");
    assert!(
        message.contains("admin listener") && message.contains(&admin_port.to_string()),
        "错误要指明 admin 端口冲突：{message}"
    );
    assert!(
        !printed.contains("[relay] listening on http://"),
        "绑定失败时不得先输出数据面横幅（不能半启动）：{printed}"
    );
    // 数据面端口没有被残留监听（进程已退出，listener 已释放）。
    let rebound = TcpListener::bind(("127.0.0.1", data_port)).expect("data port must be free");
    drop(rebound);
    drop(occupied);
    cleanup(&db);
}

#[test]
fn admin_listen_off_keeps_data_plane_only() {
    let db = tmp_path("admin-off");
    let db_arg = db.to_string_lossy().into_owned();
    let output = run(&["init", "--db", &db_arg], None);
    assert!(output.status.success(), "{}", stderr(&output));

    let data_port = free_port();
    let config = write_serve_config(&db, data_port, "off");
    let mut guard = spawn_serve(&config);
    let mut stdout = BufReader::new(guard.child.stdout.take().expect("stdout"));

    let mut banners = String::new();
    let mut data_port = None;
    // 数据面横幅在前、"admin listener disabled" 紧随其后：两条都要读到。
    for _ in 0..40 {
        let mut line = String::new();
        if stdout.read_line(&mut line).expect("read stdout") == 0 {
            break;
        }
        banners.push_str(&line);
        if line.contains("[relay] listening on http://") {
            data_port = data_port.or_else(|| port_from_banner(&line));
        }
        if data_port.is_some() && banners.contains("admin listener disabled") {
            break;
        }
    }
    let data_port = data_port.unwrap_or_else(|| panic!("没有数据面横幅：{banners}"));
    assert!(
        banners.contains("admin listener disabled"),
        "显式关闭要有明确提示：{banners}"
    );
    // 数据面可达；admin 路由没有挂载（404）。
    let response = admin_get(
        data_port,
        "/admin/login",
        &format!("127.0.0.1:{data_port}"),
        None,
    );
    assert_eq!(response.status, 404, "关闭后数据面不提供 admin 页面");

    let _ = stop(guard);
    let mut serve_stdout = banners.clone();
    let _ = stdout.read_to_string(&mut serve_stdout);
    assert!(
        !serve_stdout.contains("admin listening on http://"),
        "off 模式不得启动 admin listener：{serve_stdout}"
    );
    cleanup(&db);
}
