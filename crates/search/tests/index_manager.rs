//! S4-1c：IndexManager 生命周期与正确性测试。
//!
//! 全部使用真实 SQLite（`CARGO_TARGET_TMPDIR`）+ 真实 tantivy；不 sleep，
//! debounce 用显式 `Instant`，事件用显式 `poll_events` / `mark_dirty`。

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use menghunote_search::{
    Fingerprint, FlushOutcome, IndexManager, SearchError, SearchIndex, ACTIVE_GENERATION_META_KEY,
    DEBOUNCE, FINGERPRINT_FILE,
};
use menghunote_storage::{
    append_updates, load_body_doc, materialize_body, StructureWriter, UpdateRecord, Vault,
};

const PEER: u64 = 9_100_001;

fn tmp_vault(name: &str) -> (PathBuf, Vault) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("search-manager-{name}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("建临时目录失败");
    let path = dir.join("vault.sqlite3");
    let vault = Vault::open(&path).expect("打开 vault 失败");
    (dir, vault)
}

fn seed_note(vault: &Vault, title: &str, body: &str, now: i64) -> (String, String) {
    let mut writer = StructureWriter::open(vault, PEER).expect("打开结构写入口失败");
    let node = writer.doc().create_note(title, now).expect("创建笔记失败");
    writer.commit("seed").expect("提交结构失败");
    let (doc, head) = load_body_doc(vault, &node.body_doc_id, PEER, None).expect("加载正文失败");
    doc.insert_scalars(0, body).expect("写正文失败");
    let base = head.version_vector().expect("解码 vv 失败");
    let update = doc.pending_update(&base).expect("导出增量失败");
    append_updates(
        vault,
        &node.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("seed", update)],
    )
    .expect("写 A 事务失败");
    materialize_body(vault, &node.note_id, &node.body_doc_id, 1, PEER).expect("物化失败");
    (node.note_id, node.node_id)
}

fn rename_note(vault: &Vault, node_id: &str, title: &str, now: i64) {
    let mut writer = StructureWriter::open(vault, PEER).expect("打开结构写入口失败");
    writer.doc().rename(node_id, title, now).expect("改名失败");
    writer.commit("rename").expect("提交结构失败");
}

fn delete_note(vault: &Vault, node_id: &str, now: i64) {
    let mut writer = StructureWriter::open(vault, PEER).expect("打开结构写入口失败");
    writer.doc().delete_note(node_id, now).expect("删除失败");
    writer.commit("delete").expect("提交结构失败");
}

#[test]
fn lifecycle_create_rename_delete() {
    let (_dir, vault) = tmp_vault("lifecycle");
    let (note_id, node_id) = seed_note(&vault, "笔记流程", "这是关于笔记流程的说明", 1);
    let (_gpui_id, _) = seed_note(&vault, "GPUI 编辑器", "menghunote 使用 GPUI 构建", 2);

    let mut manager = IndexManager::open(&vault).expect("打开 IndexManager 失败");
    assert_eq!(manager.query("笔记", 10).expect("查询失败").len(), 1);

    // 改名：走 dirty 增量，标题必须立即更新。
    rename_note(&vault, &node_id, "全新标题", 3);
    manager.mark_dirty(&note_id);
    manager.flush_now().expect("flush 改名失败");
    let hits = manager.query("全新标题", 10).expect("查新标题失败");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].note_id, note_id);

    // 删除：增量删除后查询不返回，manifest 对账也不应恢复。
    let (_gpui_note, gpui_node) = seed_note(&vault, "临时文档", "只有这篇包含 独有词", 4);
    manager.reconcile_now().expect("对账新文档失败");
    assert_eq!(
        manager.query("独有词", 10).expect("查临时文档失败").len(),
        1
    );
    delete_note(&vault, &gpui_node, 5);
    manager.reconcile_now().expect("对账删除失败");
    assert!(
        manager
            .query("独有词", 10)
            .expect("删除后查询失败")
            .is_empty(),
        "墓碑必须从索引里清掉"
    );
}

#[test]
fn reconcile_repairs_missing_document_and_stale_version() {
    let (_dir, vault) = tmp_vault("reconcile");
    let (old_note, old_node) = seed_note(&vault, "初始标题", "旧正文", 1);

    let mut manager = IndexManager::open(&vault).expect("打开失败");
    assert_eq!(manager.query("旧正文", 10).expect("查询失败").len(), 1);

    // 绕过 manager 新增一篇 + 改一篇标题：索引不知道，必须靠全量对账修复。
    let (new_note, _) = seed_note(&vault, "补缺", "新增文档内容", 2);
    rename_note(&vault, &old_node, "改名后的标题", 3);

    let report = manager.reconcile_now().expect("对账失败");
    assert!(report.upserted >= 2, "应补缺 + 更新: {report:?}");
    assert_eq!(
        manager.query("新增文档内容", 10).expect("查询失败").len(),
        1
    );
    let hits = manager.query("改名后的标题", 10).expect("查询标题失败");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].note_id, old_note);
    // 新笔记也已进版本水位。
    assert!(manager.indexed_versions().contains_key(&new_note));
}

#[test]
fn fingerprint_or_pointer_switch_keeps_old_generation_usable_until_swap() {
    let (_dir, vault) = tmp_vault("generation");
    seed_note(&vault, "持久化", "切换前的正文", 1);

    let manager = IndexManager::open(&vault).expect("第一次打开失败");
    let old_id = manager.generation().id.clone();
    let old_dir = manager.generation_dir().to_path_buf();
    drop(manager);

    // 旧代在切换前仍然可独立打开并查询。
    let old_index = SearchIndex::open(&old_dir).expect("旧代应仍可打开");
    assert_eq!(
        old_index.query("持久化", 10).expect("旧代查询失败").len(),
        1
    );

    // 损坏发布指针：下一次 open 必须新建一代（旧代直到切换前仍可用）。
    vault
        .set_meta(ACTIVE_GENERATION_META_KEY, b"pointer-corrupted")
        .expect("写损坏指针失败");
    let manager = IndexManager::open(&vault).expect("指针损坏后重建失败");
    assert_ne!(manager.generation().id, old_id, "应生成新代");
    assert_eq!(manager.query("持久化", 10).expect("新代查询失败").len(), 1);
    // 旧代 reader 在切换期间仍可用（文件删除失败也不影响已打开 reader）。
    assert_eq!(
        old_index
            .query("持久化", 10)
            .expect("旧代切换后查询失败")
            .len(),
        1
    );
    drop(old_index);
    let new_id = manager.generation().id.clone();
    drop(manager);

    // 指针已写：正常重开应复用同一代，而不是每次都重建。
    let reused = IndexManager::open(&vault).expect("按指针重开失败");
    assert_eq!(reused.generation().id, new_id);
}

#[test]
fn fingerprint_mismatch_creates_new_generation() {
    let (_dir, vault) = tmp_vault("fingerprint");
    seed_note(&vault, "指纹", "指纹正文", 1);
    let manager = IndexManager::open(&vault).expect("第一次打开失败");
    let old_id = manager.generation().id.clone();
    let old_dir = manager.generation_dir().to_path_buf();
    drop(manager);

    // 用"合法但版本不同"的 fingerprint 覆盖旧代：旧代必须拒绝 open。
    let mut wrong = Fingerprint::current();
    wrong.schema_version += 1;
    fs::write(old_dir.join(FINGERPRINT_FILE), wrong.encode()).expect("写错误 fingerprint 失败");
    match SearchIndex::open(&old_dir) {
        Err(SearchError::Fingerprint(_)) => {}
        Err(other) => panic!("期望 fingerprint 错误，实际 {other:?}"),
        Ok(_) => panic!("旧 fingerprint 不应能打开"),
    }

    let manager = IndexManager::open(&vault).expect("fingerprint 不匹配后应重建");
    assert_ne!(manager.generation().id, old_id, "应生成新代");
    assert_eq!(
        manager.query("指纹正文", 10).expect("新代查询失败").len(),
        1
    );
}

#[test]
fn crash_before_pointer_keeps_old_active_generation() {
    let (dir, vault) = tmp_vault("crash-before-pointer");
    seed_note(&vault, "旧代", "旧代正文", 1);

    let manager = IndexManager::open(&vault).expect("第一次打开失败");
    let old_id = manager.generation().id.clone();
    drop(manager);

    // 模拟"新代建完但指针没写就崩溃"：留下一个孤儿代目录，指针仍指向旧代。
    let orphan = dir.join("search").join("generations").join("orphan-crash");
    fs::create_dir_all(&orphan).expect("建孤儿代目录失败");
    fs::write(orphan.join("leftover.bin"), b"orphan").expect("写孤儿文件失败");

    let manager = IndexManager::open(&vault).expect("指针未写时应复用旧代");
    assert_eq!(manager.generation().id, old_id, "必须优先使用已发布指针");
    assert_eq!(manager.query("旧代正文", 10).expect("查询失败").len(), 1);
    assert!(!orphan.exists(), "启动清理应回收未发布的孤儿代");
}

#[test]
fn missing_pointer_falls_back_to_new_generation() {
    let (_dir, vault) = tmp_vault("missing-pointer");
    seed_note(&vault, "指针", "指针正文", 1);
    let manager = IndexManager::open(&vault).expect("第一次打开失败");
    let old_id = manager.generation().id.clone();
    drop(manager);

    // 模拟"新代建好但指针还没写就崩溃"：指针缺失/空 → 下一次 open 建新代。
    vault
        .set_meta(ACTIVE_GENERATION_META_KEY, b"")
        .expect("清空指针失败");
    let manager = IndexManager::open(&vault).expect("指针缺失后应重建");
    assert_ne!(manager.generation().id, old_id);
    assert_eq!(manager.query("指针正文", 10).expect("查询失败").len(), 1);
}

#[test]
fn query_final_check_extends_candidates_past_tombstones() {
    let (_dir, vault) = tmp_vault("final-check");
    let (first, first_node) = seed_note(&vault, "第一篇", "共同词 甲", 1);
    let (second, second_node) = seed_note(&vault, "第二篇", "共同词 乙", 2);
    let (_third, _) = seed_note(&vault, "第三篇", "共同词 丙", 3);

    let manager = IndexManager::open(&vault).expect("打开失败");
    assert_eq!(manager.query("共同词", 3).expect("初始查询失败").len(), 3);

    // 绕过 manager 删除两篇：索引仍含 3 篇，但查询终检必须扩到第 3 篇。
    delete_note(&vault, &first_node, 4);
    delete_note(&vault, &second_node, 5);
    let hits = manager.query("共同词", 2).expect("终检查询失败");
    assert_eq!(hits.len(), 1, "只能返回仍存在的第三篇: {hits:?}");
    assert_ne!(hits[0].note_id, first);
    assert_ne!(hits[0].note_id, second);
}

#[test]
fn debounce_merges_dirty_and_processes_once() {
    let (_dir, vault) = tmp_vault("debounce");
    let (_note_id, _) = seed_note(&vault, "防抖", "正文", 1);
    let mut manager = IndexManager::open(&vault).expect("打开失败");

    let t0 = Instant::now();
    manager.mark_dirty_at("same-note", t0);
    manager.mark_dirty_at("same-note", t0);
    match manager.flush_if_due(t0).expect("flush 失败") {
        FlushOutcome::Deferred { .. } => {}
        other => panic!("debounce 未到期应是 Deferred，实际 {other:?}"),
    }
    match manager
        .flush_if_due(t0 + DEBOUNCE - Duration::from_millis(1))
        .expect("flush 失败")
    {
        FlushOutcome::Deferred { .. } => {}
        other => panic!("差 1ms 仍应 Deferred，实际 {other:?}"),
    }
    match manager.flush_if_due(t0 + DEBOUNCE).expect("flush 失败") {
        FlushOutcome::Applied { examined, .. } => {
            assert_eq!(examined, 1, "同 note 多次通知必须合并成一次");
        }
        other => panic!("到期应 Applied，实际 {other:?}"),
    }
    assert_eq!(manager.degraded(), None);
}

#[test]
fn storage_events_are_subscribed_and_can_be_flushed() {
    let (_dir, vault) = tmp_vault("events");
    let (note_id, node_id) = seed_note(&vault, "事件", "事件正文 旧", 1);
    let mut manager = IndexManager::open(&vault).expect("打开失败");

    rename_note(&vault, &node_id, "事件新标题", 2);
    // 订阅是同一个 storage actor 的 try_send，事件在写返回时已进入队列。
    let drained = manager.poll_events(Instant::now()).expect("poll 失败");
    assert!(drained > 0, "应收到结构变更事件");
    let report = manager.flush_now().expect("flush 事件失败");
    assert!(matches!(report, FlushOutcome::Applied { .. }));
    let hits = manager.query("事件新标题", 10).expect("查询失败");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].note_id, note_id);
    // 索引里是纯文本；标题不残留 Markdown 标记。
    assert!(!hits[0].title.contains('#'));
}

#[test]
fn open_missing_fingerprint_directory_is_recovered() {
    let (_dir, vault) = tmp_vault("recover");
    seed_note(&vault, "可恢复", "正文", 1);
    let manager = IndexManager::open(&vault).expect("第一次打开失败");
    let dir = manager.generation_dir().to_path_buf();
    drop(manager);

    // 模拟"索引目录被外部破坏"：删掉 fingerprint 文件，open 应重建而不是报错。
    fs::remove_file(dir.join(menghunote_search::FINGERPRINT_FILE)).expect("删 fingerprint 失败");
    let manager = match IndexManager::open(&vault) {
        Ok(manager) => manager,
        Err(SearchError::Tantivy(_)) | Err(SearchError::Io(_)) => {
            panic!("目录损坏应由新代兜底，而不是直接失败");
        }
        Err(other) => panic!("意外错误: {other:?}"),
    };
    assert_eq!(
        manager.query("可恢复", 10).expect("重建后查询失败").len(),
        1
    );
}
