//! 评审 0002 §五 T20：磁盘满（SQLITE_FULL）时只拒绝写、不抢删安全根，故障恢复后可续写。
//!
//! 用真实 SQLite `PRAGMA max_page_count` 把库容量钉在当前页数，制造**真实**的
//! `SQLITE_FULL`（不是 mock）；写路径仍是 relay 的 `BEGIN IMMEDIATE` 事务 + 真实配额账本。
//! 不 sleep。

mod support;

use support::*;

use relay::envelope::ObjectType;
use relay::objects;

fn page_stat(fx: &Fixture, pragma: &str) -> i64 {
    let pragma = pragma.to_owned();
    fx.db
        .read(move |conn| Ok(conn.query_row(&format!("PRAGMA {pragma}"), [], |row| row.get(0))?))
        .expect("pragma")
}

/// 把库容量钉在当前页数，返回原始上限用于恢复。
fn cap_capacity(fx: &Fixture) -> i64 {
    let original = page_stat(fx, "max_page_count");
    let pages = page_stat(fx, "page_count");
    fx.raw_write(move |conn| {
        conn.pragma_update(None, "max_page_count", pages)?;
        Ok(())
    })
    .expect("cap max_page_count");
    original
}

fn restore_capacity(fx: &Fixture, original: i64) {
    fx.raw_write(move |conn| {
        conn.pragma_update(None, "max_page_count", original)?;
        Ok(())
    })
    .expect("restore max_page_count");
}

fn parts_count(fx: &Fixture) -> i64 {
    fx.db
        .read(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM object_parts", [], |row| row.get(0))?))
        .expect("parts count")
}

fn log_count(fx: &Fixture) -> i64 {
    fx.db
        .read(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM log", [], |row| row.get(0))?))
        .expect("log count")
}

fn object_state(fx: &Fixture, object_id: &[u8; 32]) -> Option<String> {
    let object_id = *object_id;
    fx.db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT state FROM objects WHERE object_id = ?1",
                    [object_id.as_slice()],
                    |row| row.get::<_, String>(0),
                )
                .ok())
        })
        .expect("object state")
}

#[test]
fn disk_full_on_part_write_keeps_old_data_and_resumes_after_space() {
    let fx = Fixture::with_limits("fault-disk-part", 8 * 1024 * 1024, 16 * 1024 * 1024);
    let (old_object, _old_envelope, _) = fx.create_object(ObjectType::Update, 1, 1, 32);
    let usage_before = fx.usage();
    let parts_before = parts_count(&fx);
    let head_before = fx.head_seq();

    // 1 MiB 分片必须扩页；容量钉死后写事务应拿到 SQLITE_FULL 并整体回滚。
    let object_id = random_object_id();
    let envelope = support::single_part_envelope_at(
        ObjectType::Update,
        fx.vault_id,
        object_id,
        1,
        2,
        relay::envelope::NON_ASSET_CAP_PLAIN,
        1,
    );
    let original = cap_capacity(&fx);
    let error = fx
        .put_part(&envelope, None)
        .expect_err("磁盘满必须拒绝写入");
    assert!(!format!("{error:?}").is_empty());

    // 旧数据/账本/日志/对象状态全部不动：只拒绝，不抢删安全根。
    assert_eq!(fx.usage(), usage_before, "失败写不得改配额账本");
    assert_eq!(parts_count(&fx), parts_before, "失败写不得落任何 part");
    assert_eq!(fx.head_seq(), head_before, "失败写不得追加日志");
    assert!(fx.db.read(|conn| objects::load(conn, &old_object)).is_ok());
    assert_eq!(
        object_state(&fx, &object_id),
        None,
        "失败对象不得留下 objects 行"
    );
    assert_eq!(
        fx.db
            .read(|conn| {
                let result: String =
                    conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
                Ok(result)
            })
            .expect("integrity_check"),
        "ok",
        "磁盘满不得损坏库"
    );
    // 读路径/鉴权不受影响。
    assert!(!fx.device_token().is_empty());
    assert!(fx.db.read(|conn| objects::load(conn, &old_object)).is_ok());

    // 恢复容量后同一信封可续写并 complete。
    restore_capacity(&fx, original);
    fx.put_part(&envelope, None)
        .expect("容量恢复后同一片必须可重试");
    fx.complete_object(&object_id, &envelope)
        .expect("容量恢复后 complete");
    assert_eq!(fx.head_seq(), head_before + 1);
    assert_eq!(object_state(&fx, &object_id).as_deref(), Some("complete"));
    assert!(fx.usage().normal_bytes > usage_before.normal_bytes);
}

#[test]
fn complete_write_failure_keeps_part_staging_and_resumes() {
    let fx = Fixture::with_limits("fault-disk-complete", 8 * 1024 * 1024, 16 * 1024 * 1024);
    let log_before = log_count(&fx);
    let head_before = fx.head_seq();

    let object_id = random_object_id();
    let envelope = support::single_part_envelope_at(
        ObjectType::Update,
        fx.vault_id,
        object_id,
        1,
        1,
        relay::envelope::NON_ASSET_CAP_PLAIN,
        1,
    );
    fx.put_part(&envelope, None).expect("part");
    let parts_after_put = parts_count(&fx);
    let usage_after_put = fx.usage();
    assert_eq!(object_state(&fx, &object_id).as_deref(), Some("staging"));

    // complete 的最后一步（写日志）失败：真实事务回滚，staging part 保留。
    fx.raw_write(|conn| {
        conn.execute_batch(
            "CREATE TRIGGER fail_complete_log BEFORE INSERT ON log \
             BEGIN SELECT RAISE(ABORT,'injected log'); END;",
        )?;
        Ok(())
    })
    .expect("注入日志失败");
    let error = fx
        .complete_object(&object_id, &envelope)
        .expect_err("complete 写失败必须返回错误");
    assert!(!format!("{error:?}").is_empty());

    assert_eq!(fx.head_seq(), head_before, "失败 complete 不得进日志");
    assert_eq!(log_count(&fx), log_before, "失败 complete 不得追加日志行");
    assert_eq!(parts_count(&fx), parts_after_put, "已落 part 不得被删除");
    assert_eq!(object_state(&fx, &object_id).as_deref(), Some("staging"));
    assert_eq!(
        fx.usage(),
        usage_after_put,
        "complete 失败不得改动已在 part 阶段入账的配额"
    );

    // 撤销注入后重试 complete：同一对象只产生一条日志，staging → complete。
    fx.raw_write(|conn| {
        conn.execute_batch("DROP TRIGGER fail_complete_log")?;
        Ok(())
    })
    .expect("撤销注入");
    fx.complete_object(&object_id, &envelope)
        .expect("恢复后 complete");
    assert_eq!(fx.head_seq(), head_before + 1);
    assert_eq!(log_count(&fx), log_before + 1);
    assert_eq!(object_state(&fx, &object_id).as_deref(), Some("complete"));
}

/// 配额拒绝（Q 满）同样只拒绝、不删除已存在对象；恢复额度后继续写。
#[test]
fn quota_exhaustion_is_reject_only_and_resumable() {
    let fx = Fixture::with_limits("fault-quota-resume", 300, 600);
    let (first, _, _) = fx.create_object(ObjectType::Update, 1, 1, 50);
    let before = fx.usage();
    let parts_before = parts_count(&fx);

    let object_id = random_object_id();
    let envelope =
        support::single_part_envelope_at(ObjectType::Update, fx.vault_id, object_id, 1, 1, 50, 1);
    let error = fx.put_part(&envelope, None).expect_err("Q 满必须 507");
    assert_eq!(
        error.code().map(|code| format!("{code:?}")),
        Some("QuotaExceeded".to_owned())
    );
    assert_eq!(fx.usage(), before);
    assert_eq!(parts_count(&fx), parts_before);
    assert!(
        fx.db.read(|conn| objects::load(conn, &first)).is_ok(),
        "不得删旧对象"
    );

    // 提高配额（模拟运维扩容）后同一片可写入 —— 故障可 resume。
    fx.db
        .write_blocking(|conn| {
            conn.execute(
                "UPDATE vaults SET normal_limit = 1200, maintenance_limit = 1200
                 WHERE vault_id = ?1",
                [conn.scope().as_slice()],
            )?;
            Ok(())
        })
        .expect("raise quota");
    fx.put_part(&envelope, None).expect("扩容后同一片必须可写");
    assert!(fx.usage().normal_bytes > before.normal_bytes);
}
