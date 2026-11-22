//! P2-6：`meta.value` 的三个读取入口必须只有**一套**语义 —— 只认 BLOB。
//!
//! | 入口 | 实际代码路径 |
//! | --- | --- |
//! | [`menghunote_storage::Vault::get_meta`] | actor 线程上的 `GetMeta` 命令 |
//! | `sync_bridge::read_meta` | `sync_cursor_state` / `take_inbox_ready` / `set_sync_log_id` 等同步桥读取 |
//! | `sync_meta::read_meta_on` | worker 独占连接 / 批量 inbox 事务内的读取 |
//!
//! 为什么专门测非 BLOB：`meta.value` 是 `BLOB NOT NULL`（`schema/v1.sql` 里的 STRICT
//! 表），正常写入路径**写不出** TEXT/INTEGER/REAL。能读出来只可能是"库被外部工具改坏
//! 或 schema 漂移"，而旧行为对它有两种互相矛盾的解读：
//!
//! - `Vault::get_meta` 把 TEXT 当字节返回；
//! - `sync_bridge::read_meta` / `sync_meta::read_meta_on` 把 TEXT 当**缺失**。
//!
//! 后者会静默绕过持久屏障与游标：`sync.log_id` 读不到就当"还没绑定 incarnation"
//! （`take_inbox_ready` 静默返回空，`set_sync_log_id` 还会把损坏值覆盖掉，等于悄悄
//! 换绑传代），`sync.repack_required` 读不到就当"不需要 repack"。
//!
//! 测试库用 `PRAGMA writable_schema` 把 `meta` 表改回非 STRICT，从而如实构造出这种
//! 损坏状态（STRICT 表拒绝非 BLOB 写入，测试里没有别的办法造出 TEXT）。

mod support;

use std::path::Path;

use support::*;

use menghunote_storage::{
    advance_integrated_prefix, repack_required, set_repack_required, set_sync_log_id,
    sync_cursor_state, take_inbox_ready, StorageError, Value, Vault, META_SYNC_LOG_ID,
    META_SYNC_REPACK_REQUIRED,
};

/// 测试用的合法 relay log incarnation（16B BLOB）。
const LOG_ID: [u8; 16] = [0x2f; 16];

/// 把测试库的 `meta` 表从 STRICT 改成非 STRICT。
///
/// 只改 `sqlite_master` 里那条 DDL（去掉唯一的 `STRICT` 后缀），其余列定义逐字保留；
/// 之后的连接会按非 STRICT 解析这张表，于是能写入 TEXT/INTEGER/REAL —— 正是"库被外部
/// 改坏"的形状。
fn relax_meta_table(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("打开原始连接");
    conn.execute_batch("PRAGMA writable_schema=ON")
        .expect("打开 writable_schema");
    let ddl: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='meta'",
            [],
            |row| row.get(0),
        )
        .expect("读 meta 表 DDL");
    let relaxed = ddl.replacen(" STRICT", "", 1);
    assert_ne!(relaxed, ddl, "meta 表 DDL 应带 STRICT：{ddl}");
    conn.execute(
        "UPDATE sqlite_master SET sql=?1 WHERE name='meta'",
        [relaxed],
    )
    .expect("改写 meta 表 DDL");
    conn.execute_batch("PRAGMA writable_schema=OFF")
        .expect("关闭 writable_schema");
    drop(conn);
}

/// 建一个 vault，把 `meta` 表改成非 STRICT 后重新打开。
///
/// 重新打开走 [`Vault::open`]：`user_version` 已是 `SCHEMA_VERSION`，迁移不会重跑，
/// 所以非 STRICT 的 `meta` 表被原样沿用。
fn vault_with_typed_meta() -> (tempfile::TempDir, Vault) {
    let dir = temp_dir();
    let path = vault_path(&dir);
    {
        let vault = open_vault(&path);
        // 先写一版合法 BLOB，确保 schema 与 WAL 都已落盘，再改表结构。
        set_sync_log_id(&vault, &LOG_ID).expect("预置合法 BLOB");
    }
    relax_meta_table(&path);
    let vault = open_vault(&path);
    (dir, vault)
}

/// 把一个 meta 键写成指定 SQLite 类型（绕过 Vault 的 BLOB 写入路径）。
fn write_typed_meta(vault: &Vault, key: &str, value: Value) {
    vault
        .exec(
            "INSERT INTO meta(key,value) VALUES(?1,?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            &[Value::Text(key.to_owned()), value],
        )
        .expect("写入指定类型的 meta");
}

/// `meta.value` 的实际 SQLite 类型名（`typeof()`，用于确认构造出的损坏状态）。
fn value_type(vault: &Vault, key: &str) -> String {
    let rows = vault
        .query(
            "SELECT typeof(value) FROM meta WHERE key=?1",
            &[Value::Text(key.to_owned())],
        )
        .expect("读 typeof(value)");
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Text(name)) => name.clone(),
        other => panic!("typeof(value) 类型异常：{other:?}"),
    }
}

/// 断言结果是 [`StorageError::Corrupt`]，返回错误上下文（便于继续断言 key 与实际类型）。
fn corrupt<T: std::fmt::Debug>(result: Result<T, StorageError>, label: &str) -> String {
    match result {
        Ok(value) => panic!("{label} 必须报损坏，实际 Ok({value:?})"),
        Err(StorageError::Corrupt { context }) => context,
        Err(other) => panic!("{label} 必须是 Corrupt，实际 {other:?}"),
    }
}

/// 同一 key 写成 TEXT：三个入口必须给出**同一个**损坏结论。
#[test]
fn text_meta_is_corrupt_from_all_three_entry_points() {
    let (_dir, vault) = vault_with_typed_meta();
    write_typed_meta(&vault, META_SYNC_LOG_ID, Value::Text("2f".repeat(16)));
    assert_eq!(
        value_type(&vault, META_SYNC_LOG_ID),
        "text",
        "构造出的值必须是 TEXT"
    );

    // 入口 1：Vault::get_meta（actor 线程）。旧行为把 TEXT 直接当字节返回。
    let from_actor = corrupt(vault.get_meta(META_SYNC_LOG_ID), "Vault::get_meta");
    // 入口 2：sync_bridge::read_meta（sync_cursor_state 读 sync.log_id）。旧行为当缺失。
    let from_bridge = corrupt(sync_cursor_state(&vault), "sync_bridge::read_meta");
    // 入口 3：sync_meta::read_meta_on（前缀推进在 worker 连接上读 sync.log_id）。
    let from_meta = corrupt(advance_integrated_prefix(&vault), "sync_meta::read_meta_on");

    for (label, context) in [
        ("Vault::get_meta", &from_actor),
        ("sync_bridge::read_meta", &from_bridge),
        ("sync_meta::read_meta_on", &from_meta),
    ] {
        assert!(
            context.contains(META_SYNC_LOG_ID),
            "{label} 的错误信息必须点名 key：{context}"
        );
        assert!(
            context.contains("TEXT") && context.contains("BLOB"),
            "{label} 的错误信息必须同时给出实际类型与期望类型：{context}"
        );
    }
    // 三处共用同一判定函数，连错误文案都不该有差别。
    assert_eq!(
        from_actor, from_bridge,
        "actor 与 sync_bridge 的结论必须一致"
    );
    assert_eq!(from_actor, from_meta, "actor 与 sync_meta 的结论必须一致");
}

/// BLOB（包括空 BLOB）继续按原语义读出，前缀推进不受影响。
#[test]
fn blob_meta_reads_consistently_from_all_three_entry_points() {
    let (_dir, vault) = fresh_vault();
    set_sync_log_id(&vault, &LOG_ID).expect("写 BLOB sync.log_id");

    assert_eq!(
        vault.get_meta(META_SYNC_LOG_ID).expect("读 BLOB meta"),
        Some(LOG_ID.to_vec()),
        "BLOB 必须逐字节读出"
    );
    let state = sync_cursor_state(&vault).expect("读游标状态");
    assert_eq!(
        state.log_id,
        Some(LOG_ID),
        "sync_bridge 必须读到同一个 BLOB"
    );
    assert_eq!(state.received_seq, 0);
    assert_eq!(state.integrated_seq, 0);
    assert_eq!(state.confirmed_highwater, None);

    advance_integrated_prefix(&vault).expect("BLOB 下前缀推进是合法 no-op");

    set_repack_required(&vault, true).expect("写 repack 屏障");
    assert!(
        repack_required(&vault).expect("读 repack 屏障"),
        "BLOB 屏障必须生效"
    );
    set_repack_required(&vault, false).expect("清 repack 屏障");
    assert!(!repack_required(&vault).expect("读 repack 屏障"));
}

/// 缺失的键在三个入口都是"没有"，不是损坏。
#[test]
fn missing_meta_reads_as_absent_from_all_three_entry_points() {
    let (_dir, vault) = fresh_vault();

    assert_eq!(vault.get_meta(META_SYNC_LOG_ID).expect("读缺失 key"), None);
    let state = sync_cursor_state(&vault).expect("读游标状态");
    assert_eq!(state.log_id, None);
    assert_eq!(state.received_seq, 0);
    assert_eq!(state.integrated_seq, 0);
    assert_eq!(state.confirmed_highwater, None);
    advance_integrated_prefix(&vault).expect("缺失时前缀推进是 no-op");
    assert!(!repack_required(&vault).expect("缺失屏障读成 false"));
    assert_eq!(
        vault
            .get_meta(META_SYNC_REPACK_REQUIRED)
            .expect("读缺失屏障"),
        None
    );
    assert!(
        take_inbox_ready(&vault, 8)
            .expect("没有 incarnation 时 ready 为空")
            .is_empty(),
        "缺失 sync.log_id 时 ready 必须为空（不回退成不过滤 log）"
    );
}

/// 损坏的 `sync.log_id` 不能被当成"还没绑定 incarnation"静默吞掉。
///
/// 这是 P2-6 的实际风险点：旧行为下 `take_inbox_ready` 会静默返回空（pull 永久卡住），
/// `set_sync_log_id` 会把损坏值覆盖成新值（悄悄换绑传代，旧行混进新代）。
#[test]
fn text_log_id_blocks_ready_and_rebinding_instead_of_silently_passing() {
    let (_dir, vault) = vault_with_typed_meta();
    write_typed_meta(&vault, META_SYNC_LOG_ID, Value::Text("2f".repeat(16)));

    let context = corrupt(take_inbox_ready(&vault, 8), "take_inbox_ready");
    assert!(
        context.contains(META_SYNC_LOG_ID),
        "错误必须点名 sync.log_id：{context}"
    );
    let context = corrupt(set_sync_log_id(&vault, &LOG_ID), "set_sync_log_id");
    assert!(
        context.contains(META_SYNC_LOG_ID),
        "错误必须点名 sync.log_id：{context}"
    );

    // 损坏值仍在库里：没有被"缺失 → 写入新版"的路径覆盖掉。
    assert_eq!(value_type(&vault, META_SYNC_LOG_ID), "text");
}

/// INTEGER / REAL / TEXT 全部按损坏处理（不是只有 TEXT 特殊）。
#[test]
fn non_blob_meta_types_are_corrupt() {
    let (_dir, vault) = vault_with_typed_meta();
    // NULL 造不出来：`meta.value` 是 NOT NULL，非 STRICT 表同样拒绝。
    for (value, expected_type) in [
        (Value::Integer(7), "INTEGER"),
        (Value::Real(1.5), "REAL"),
        (Value::Text("7".to_owned()), "TEXT"),
    ] {
        write_typed_meta(&vault, META_SYNC_LOG_ID, value);
        assert_eq!(
            value_type(&vault, META_SYNC_LOG_ID),
            expected_type.to_lowercase()
        );
        let context = corrupt(vault.get_meta(META_SYNC_LOG_ID), "Vault::get_meta");
        assert!(
            context.contains(expected_type),
            "错误必须报实际类型 {expected_type}：{context}"
        );
        let context = corrupt(sync_cursor_state(&vault), "sync_bridge::read_meta");
        assert!(
            context.contains(expected_type),
            "sync_bridge 也必须报实际类型 {expected_type}：{context}"
        );
    }
}

/// 屏障键本身：损坏的 `sync.repack_required` 不得读成"不需要 repack"。
#[test]
fn repack_barrier_is_not_silently_false_when_corrupt() {
    let (_dir, vault) = vault_with_typed_meta();
    write_typed_meta(
        &vault,
        META_SYNC_REPACK_REQUIRED,
        Value::Text("1".to_owned()),
    );

    let context = corrupt(repack_required(&vault), "repack_required");
    assert!(
        context.contains(META_SYNC_REPACK_REQUIRED),
        "错误必须点名屏障 key：{context}"
    );
}
