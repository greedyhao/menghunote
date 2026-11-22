//! 生产 CLI 的进程级测试：真跑 `menghunote-relay` 二进制（不绑生产端口、不联网）。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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
    assert!(printed.contains("devices=0"), "{printed}");
    assert!(printed.contains("schema_version=9"), "{printed}");
    assert!(printed.contains("instance_id="), "{printed}");
    assert!(!printed.contains("setup_secret"), "status 不得回显 secret");

    // 第二次 init 必须拒绝（setup secret 只生成一次）。
    let output = run(&["init", "--db", &db_arg], None);
    assert!(!output.status.success(), "second init must fail");
    assert!(stderr(&output).contains("init"), "{}", stderr(&output));

    cleanup(&db);
}

#[test]
fn delete_vault_requires_yes_and_manual_confirmation() {
    let db = tmp_path("delete");
    let db_arg = db.to_string_lossy().into_owned();
    let output = run(&["init", "--db", &db_arg], None);
    assert!(output.status.success(), "{}", stderr(&output));

    // 缺 --yes → 拒绝。
    let output = run(&["delete-vault", "--db", &db_arg], None);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("--yes"), "{}", stderr(&output));
    assert!(db.exists(), "缺 --yes 不能删库");

    // --yes + 错误确认词 → 取消。
    let output = run(&["delete-vault", "--db", &db_arg, "--yes"], Some("nope\n"));
    assert!(!output.status.success());
    assert!(stderr(&output).contains("取消"), "{}", stderr(&output));
    assert!(db.exists(), "确认词不匹配不能删库");

    // --yes + DELETE → 真删。
    let output = run(
        &["delete-vault", "--db", &db_arg, "--yes"],
        Some("DELETE\n"),
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!db.exists(), "库文件应被删除");
    assert!(stdout(&output).contains("deleted="), "{}", stdout(&output));

    // 已删除的库再 status → 明确报错。
    let output = run(&["status", "--db", &db_arg], None);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("数据库不存在"),
        "{}",
        stderr(&output)
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

    // 相对路径 db。
    let config = write_config(
        "relative-db.toml",
        "listen = \"127.0.0.1:18080\"\ndb = \"relay.sqlite3\"\n",
    );
    let output = run(&["serve", "--config", &config.to_string_lossy()], None);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("绝对路径"), "{}", stderr(&output));

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
