//! 测试共用脚手架（临时目录、vault、S0 用的 notes/loro_docs 行）。
//!
//! 临时目录一律放在 cargo 给集成测试准备的 `target/tmp` 下：
//! 本机构建环境里 cargo 启动的测试进程对 `%TEMP%` 没有写权限（os error 5）。
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use menghunote_storage::{
    append_updates, create_body_doc, create_note_slot, load_head, BodyDoc, HeadState,
    PendingUpdate, StorageError, UpdateRecord, Vault,
};
use tempfile::TempDir;

/// 每台测试设备固定的 peer id（真机上必须唯一，测试里只要区分得开）。
pub const PEER_A: u64 = 1;
/// 第二台测试设备的 peer id。
pub const PEER_B: u64 = 2;

/// 在 `target/tmp` 下建一个临时目录。
pub fn temp_dir() -> TempDir {
    let base = Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(base).expect("创建 target/tmp");
    tempfile::Builder::new()
        .prefix("s0a-")
        .tempdir_in(base)
        .expect("在 target/tmp 下创建临时目录")
}

/// 临时目录里的 vault 路径（父目录故意不存在）。
pub fn vault_path(dir: &TempDir) -> PathBuf {
    dir.path().join("vaults").join("primary.sqlite3")
}

/// 打开（或创建）vault。
pub fn open_vault(path: &Path) -> Vault {
    Vault::open(path).expect("打开 vault 应当成功")
}

/// 建一个临时目录 + vault。
pub fn fresh_vault() -> (TempDir, Vault) {
    let dir = temp_dir();
    let vault = open_vault(&vault_path(&dir));
    (dir, vault)
}

/// 建好 `loro_docs` 行与 `notes` 行，并返回一个空正文文档。
pub fn seed_note(vault: &Vault, note_id: &str, doc_id: &str) -> BodyDoc {
    create_body_doc(vault, doc_id).expect("插入 loro_docs 行");
    create_note_slot(vault, note_id, doc_id, "测试笔记", 1).expect("插入 notes 行");
    BodyDoc::create(doc_id, PEER_A).expect("创建 BodyDoc")
}

/// 读 head，失败直接 panic（测试语境）。
pub fn head(vault: &Vault, doc_id: &str) -> HeadState {
    load_head(vault, doc_id).expect("读取 head")
}

/// 把一次提交推进到 A 事务（从库里当前 head 出发）。
pub fn commit_now(vault: &Vault, doc: &BodyDoc, batch_id: &str) -> HeadState {
    let current = head(vault, doc.doc_id());
    let base = current.version_vector().expect("解码 head_vv");
    let update = doc.pending_update(&base).expect("导出增量");
    append_updates(
        vault,
        doc.doc_id(),
        current.head_seq,
        &[UpdateRecord::new(batch_id, update)],
    )
    .expect("A 事务")
}

/// 手工构造一条待落库增量（用于 journal / 故障注入测试）。
pub fn pending(doc: &BodyDoc, base: &loro::VersionVector) -> Result<PendingUpdate, StorageError> {
    doc.pending_update(base)
}

/// 查询单个整数（测试断言用）。
pub fn count(vault: &Vault, sql: &str, params: &[menghunote_storage::Value]) -> i64 {
    let rows = vault.query(sql, params).expect("查询应成功");
    match rows.first().and_then(|row| row.first()).cloned() {
        Some(menghunote_storage::Value::Integer(value)) => value,
        other => panic!("期望整数结果，实际 {other:?}"),
    }
}
