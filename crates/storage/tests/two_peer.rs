//! 两 peer 并发：收敛、local undo 不回退远端字符、redo 跨 remote import。

use loro::VersionVector;
use menghunote_storage::BodyDoc;

fn export_all(doc: &BodyDoc) -> Vec<u8> {
    doc.export_all_updates().expect("导出全量 update")
}

/// 双向同步（各自导入对方的全量历史，幂等）。
fn sync(a: &BodyDoc, b: &BodyDoc) {
    let a_bytes = export_all(a);
    let b_bytes = export_all(b);
    a.import(&b_bytes).expect("a 导入 b");
    b.import(&a_bytes).expect("b 导入 a");
}

#[test]
fn two_peers_converge_after_concurrent_edits() {
    let a = BodyDoc::create("doc-1", 1).expect("peer a");
    a.insert_scalars(0, "A中").expect("基准文本");
    let b = BodyDoc::create("doc-1", 2).expect("peer b");
    b.import(&export_all(&a)).expect("b 从 a 起步");

    // 并发编辑：a 在尾部加中文，b 在头部加 emoji
    a.insert_scalars(a.scalar_len(), "甲").expect("a 编辑");
    b.insert_scalars(0, "😀").expect("b 编辑");
    assert_ne!(a.text(), b.text(), "同步前两边本来就不同");

    sync(&a, &b);

    assert_eq!(a.text(), b.text(), "交换 update 后必须收敛");
    assert_eq!(a.frontiers(), b.frontiers(), "收敛后 frontier 也一致");
    assert!(a.text().contains('甲'), "a 的编辑不能丢");
    assert!(a.text().contains('😀'), "b 的编辑不能丢");
}

#[test]
fn concurrent_delete_and_insert_keep_both_intents() {
    let a = BodyDoc::create("doc-1", 1).expect("peer a");
    a.insert_scalars(0, "A中😀e\u{301}").expect("基准文本");
    let b = BodyDoc::create("doc-1", 2).expect("peer b");
    b.import(&export_all(&a)).expect("b 起步");

    // a 删掉 `中`（scalar 位置 1）；b 在尾部插入
    a.delete_scalars(1, 1).expect("a 删除");
    b.insert_scalars(b.scalar_len(), "!").expect("b 插入");
    sync(&a, &b);

    assert_eq!(a.text(), b.text());
    assert!(!a.text().contains('中'), "a 的删除生效");
    assert!(a.text().ends_with('!'), "b 的插入保留");
}

#[test]
fn local_undo_does_not_revert_remote_characters() {
    let a = BodyDoc::create("doc-1", 1).expect("peer a");
    a.insert_scalars(0, "base").expect("基准文本");
    let b = BodyDoc::create("doc-1", 2).expect("peer b");
    b.import(&export_all(&a)).expect("b 起步");

    // a 的本地编辑单独成组，便于精确 undo
    let mut undo = a.undo_manager();
    undo.set_merge_interval(0);
    undo.group_start().expect("group_start");
    a.insert_scalars(a.scalar_len(), "甲").expect("a 本地编辑");
    undo.group_end();

    // b 并发写（此时两边还没同步）
    b.insert_scalars(0, "乙").expect("b 远端编辑");
    sync(&a, &b);
    assert_eq!(a.text(), b.text());
    assert!(a.text().contains('乙'));

    // a 本地 undo：只撤自己的 `甲`，远端 `乙` 必须留下
    let undone = undo.undo().expect("undo");
    assert!(undone, "本地编辑应可撤销");
    assert!(!a.text().contains('甲'), "undo 应撤掉本地字符");
    assert!(a.text().contains('乙'), "undo 不得回退远端字符");

    // 远端导入 undo 结果后同样一致
    b.import(&export_all(&a)).expect("b 导入 undo");
    assert_eq!(a.text(), b.text(), "undo 也要跨 peer 收敛");
}

#[test]
fn redo_after_remote_import_restores_only_local_changes() {
    let a = BodyDoc::create("doc-1", 1).expect("peer a");
    a.insert_scalars(0, "base").expect("基准文本");
    let b = BodyDoc::create("doc-1", 2).expect("peer b");
    b.import(&export_all(&a)).expect("b 起步");

    let mut undo = a.undo_manager();
    undo.set_merge_interval(0);
    undo.group_start().expect("group_start");
    a.insert_scalars(a.scalar_len(), "甲").expect("a 本地编辑");
    undo.group_end();

    b.insert_scalars(0, "乙").expect("b 并发编辑");
    sync(&a, &b);

    assert!(undo.undo().expect("undo"));
    assert!(a.text().contains('乙') && !a.text().contains('甲'));

    // redo：把本地 `甲` 加回来，但不会把远端 `乙` 弄丢或重复
    assert!(undo.redo().expect("redo"), "redo 应可用");
    assert!(a.text().contains('甲'), "redo 恢复本地字符");
    assert_eq!(a.text().matches('乙').count(), 1, "redo 不能让远端字符重复");
    assert!(a.text().contains('乙'));

    b.import(&export_all(&a)).expect("b 导入 redo");
    assert_eq!(a.text(), b.text());
}

#[test]
fn duplicate_import_is_idempotent_and_no_op_updates_are_detected() {
    let a = BodyDoc::create("doc-1", 1).expect("peer a");
    a.insert_scalars(0, "甲乙").expect("编辑");

    let snapshot = a.export_snapshot().expect("快照");
    let empty = VersionVector::default();
    let updates = a.pending_update(&empty).expect("增量");

    let b = BodyDoc::create("doc-1", 2).expect("peer b");
    for _ in 0..3 {
        b.import(&snapshot).expect("重复导入快照幂等");
        b.import(&updates.payload).expect("重复导入增量幂等");
    }
    assert_eq!(b.text(), "甲乙");
    assert_eq!(a.frontiers(), b.frontiers(), "幂等导入不改变 frontier");
}
