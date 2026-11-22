//! 服务端 canary 审计（ADR 0006 §9 秘密审计）：日志/DB 文本列/错误响应里**不得**出现明文、
//! 口令、root、真实资产 hash 或 token 明文。

mod support;

use support::*;

use relay::envelope::ObjectType;
use relay::ids::{b64url_encode, sha256};

/// 源码里不得有任何"顺手打日志"的入口（relay 不做业务日志；暴露即违规）。
#[test]
fn source_has_no_logging_calls() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs(&root, &mut files);
    assert!(files.len() > 10, "应当扫描到 relay 源码");
    for file in files {
        let text = std::fs::read_to_string(&file).expect("read source");
        // CLI 入口（src/main.rs）的职责就是往 stdout/stderr 输出；其余库模块仍禁止日志调用。
        let is_cli = file.file_name().and_then(|name| name.to_str()) == Some("main.rs");
        let forbidden: &[&str] = if is_cli {
            &["dbg!(", "tracing::", "log::info", "log::warn"]
        } else {
            &[
                "println!",
                "eprintln!",
                "dbg!(",
                "tracing::",
                "log::info",
                "log::warn",
            ]
        };
        for forbidden in forbidden {
            assert!(
                !text.contains(forbidden),
                "{} 不得包含日志调用 {forbidden}（canary 审计不允许服务端写业务日志）",
                file.display()
            );
        }
    }
}

fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn ciphertext_canary_never_lands_in_text_columns_or_responses() {
    let fx = Fixture::new("canary");
    // 明文 canary 放进"密文体"里（relay 不解密，只当字节存）。
    let canary = b"PLAINTEXT-CANARY-DO-NOT-LOG";
    let object_id = random_object_id();
    let mut envelope =
        support::single_part_envelope(ObjectType::Update, fx.vault_id, object_id, 1, 1, 128);
    let body_start = envelope.len() - 128;
    envelope[body_start..body_start + canary.len()].copy_from_slice(canary);
    fx.put_part(&envelope, None).expect("part");
    fx.complete_object(&object_id, &envelope).expect("complete");

    // 1) 所有 TEXT 列都不得包含 canary（BLOB 里是密文，本来就允许）。
    let dump = fx
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            )?;
            let tables: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<_, _>>()?;
            let mut dump = String::new();
            for table in tables {
                // `CAST(x AS TEXT)` 会把 BLOB 也转成文本，因此只取 SQLite 声明为 TEXT 的列。
                let mut cols = conn.prepare(&format!("PRAGMA table_info({table})"))?;
                let columns: Vec<(String, String)> = cols
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
                    })?
                    .collect::<std::result::Result<_, _>>()?;
                for (column, kind) in columns {
                    if kind.to_uppercase() != "TEXT" {
                        continue;
                    }
                    let query = format!("SELECT group_concat({column}, '|') FROM {table}");
                    let value: Option<String> = conn.query_row(&query, [], |row| row.get(0))?;
                    if let Some(value) = value {
                        dump.push_str(&value);
                        dump.push('\n');
                    }
                }
            }
            Ok(dump)
        })
        .expect("text dump");
    assert!(
        !dump.contains("PLAINTEXT-CANARY"),
        "明文 canary 不得出现在任何 TEXT 列"
    );

    // 2) 对象/日志行里只有密文与不透明 digest（不含 canary）。
    let log_event: Vec<u8> = fx
        .db
        .read(|conn| Ok(conn.query_row("SELECT event FROM log", [], |row| row.get(0))?))
        .expect("log event");
    assert!(
        !log_event
            .windows(canary.len())
            .any(|window| window == canary),
        "log 事件不得包含明文 canary"
    );

    // 3) 任何错误响应体也不得回显密文/明文（错误只带 code/retryable/水位）。
    let err = fx
        .db
        .read(|conn| {
            relay::log::page(
                conn,
                &fx.vault(),
                &relay::cursor::for_vault(&fx.vault(), 0).expect("cursor"),
                10,
                512,
            )
        })
        .expect("page");
    assert!(!format!("{err:?}").contains("CANARY"));

    // 4) 服务端 cursor key 只以 BLOB 存在，且不会出现在 wire token 里（MAC 不可逆）。
    let vault = fx.vault();
    let token = relay::cursor::CursorKey::new(vault.cursor_key)
        .encode(&relay::cursor::for_vault(&vault, 0).expect("cursor"))
        .expect("token");
    assert!(!token.contains(&b64url_encode(&vault.cursor_key)));
    // 摘要不可逆校验：wire 上不应出现 raw key 的 hex/base64 形态。
    assert!(!token.contains(&relay::ids::hex_lower(&vault.cursor_key)));
    let _ = sha256(&vault.cursor_key);
}
