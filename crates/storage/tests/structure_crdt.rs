//! 结构 CRDT：树、meta、标签字典、删除/恢复事件、两副本并发收敛。

use std::collections::HashMap;

use menghunote_storage::{normalize_tag, NodeSnapshot, StructureDoc};

fn doc(peer: u64) -> StructureDoc {
    StructureDoc::create(peer).expect("创建结构文档")
}

/// 双向同步全量 update（幂等）。
fn sync(a: &StructureDoc, b: &StructureDoc) {
    let a_bytes = a.export_all_updates().expect("导出 a");
    let b_bytes = b.export_all_updates().expect("导出 b");
    a.import(&b_bytes).expect("a 导入 b");
    b.import(&a_bytes).expect("b 导入 a");
}

fn node(doc: &StructureDoc, node_id: &str) -> NodeSnapshot {
    doc.node_snapshot(node_id).expect("读节点快照")
}

/// 树必须无环：每个节点沿 parent 上溯都能到根。
fn assert_acyclic(snapshot: &menghunote_storage::StructureSnapshot) {
    let parents: HashMap<&str, Option<&str>> = snapshot
        .nodes
        .iter()
        .map(|node| (node.node_id.as_str(), node.parent.as_deref()))
        .collect();
    for node in &snapshot.nodes {
        let mut current = Some(node.node_id.as_str());
        let mut steps = 0usize;
        while let Some(id) = current {
            current = *parents
                .get(id)
                .unwrap_or_else(|| panic!("parent 指向不存在的节点 {id}"));
            steps += 1;
            assert!(steps <= snapshot.nodes.len() + 1, "树出现环：{id}");
        }
    }
}

#[test]
fn create_note_writes_full_meta_and_containers() {
    let doc = doc(1);
    let seed = doc.create_note("我的笔记", 1_000).expect("建笔记");

    let snapshot = doc.snapshot().expect("树快照");
    assert_eq!(snapshot.nodes.len(), 1);
    let node = &snapshot.nodes[0];
    assert_eq!(node.node_id, seed.node_id);
    assert_eq!(node.parent, None, "新笔记是根级节点");
    assert_eq!(node.index, 0);
    assert_eq!(node.note_id.as_deref(), Some(seed.note_id.as_str()));
    assert_eq!(node.body_doc_id.as_deref(), Some(seed.body_doc_id.as_str()));
    assert_eq!(node.title, "我的笔记");
    assert_eq!(node.created_at, 1_000);
    assert_eq!(node.edited_at, 1_000);
    assert!(!node.is_favorite);
    assert!(!node.is_pinned);
    assert!(!node.deleted);
    assert!(node.tags.is_empty());
    assert!(node.delete_events.is_empty());

    // note_id / body_doc_id 规则：body 文档 ID 派生自 note_id，且互不相同
    assert!(seed.note_id.starts_with("note-1-"));
    assert_eq!(seed.body_doc_id, format!("{}-body", seed.note_id));

    let other = doc.create_note("第二篇", 1_100).expect("建第二篇");
    assert_ne!(other.note_id, seed.note_id, "note_id 必须唯一");
    assert_ne!(other.body_doc_id, seed.body_doc_id);
}

#[test]
fn rename_move_tags_favorite_pinned() {
    let doc = doc(1);
    let leaf = doc.create_note("叶子", 100).expect("建叶子");
    let parent = doc.create_note("父节点", 200).expect("建父节点");

    doc.rename(&leaf.node_id, "叶子（改名）", 300)
        .expect("改名");
    // parent（第二个根）挂到 leaf 下面
    doc.move_node(&parent.node_id, Some(&leaf.node_id), 400)
        .expect("移动");

    let leaf_node = node(&doc, &leaf.node_id);
    assert_eq!(leaf_node.title, "叶子（改名）");
    assert_eq!(leaf_node.edited_at, 300);

    let moved = node(&doc, &parent.node_id);
    assert_eq!(moved.parent.as_deref(), Some(leaf.node_id.as_str()));
    assert_eq!(moved.index, 0, "只有一个子节点，rank 为 0");
    assert_eq!(node(&doc, &leaf.node_id).index, 0, "移走一个后根级只剩它");

    // 标签：归一化后同名复用同一个 tag_id
    let tag_a = doc.add_tag(&leaf.node_id, "  工作  ", 500).expect("加标签");
    let tag_b = doc
        .add_tag(&leaf.node_id, "工作", 510)
        .expect("再加同一个标签");
    assert_eq!(tag_a, tag_b, "同名标签必须复用 tag_id");
    assert_eq!(normalize_tag("  工作  "), "工作");

    let tag_c = doc
        .add_tag(&leaf.node_id, "生活", 520)
        .expect("加第二个标签");
    assert_ne!(tag_a, tag_c);
    let leaf_node = node(&doc, &leaf.node_id);
    assert_eq!(leaf_node.tags.len(), 2, "集合语义：重复加不产生重复项");

    // 字典里也应只有两项
    let snapshot = doc.snapshot().expect("快照");
    assert_eq!(snapshot.tags.len(), 2);
    let names: Vec<&str> = snapshot.tags.iter().map(|tag| tag.name.as_str()).collect();
    assert!(
        names.contains(&"工作") && names.contains(&"生活"),
        "显示名按 trim 后存：{names:?}"
    );

    // 移除标签只删成员，不删字典
    doc.remove_tag(&leaf.node_id, &tag_c, 600)
        .expect("移除标签");
    assert_eq!(node(&doc, &leaf.node_id).tags.len(), 1);
    assert_eq!(doc.snapshot().expect("快照").tags.len(), 2);

    // 收藏 / 置顶
    doc.set_favorite(&leaf.node_id, true, 700).expect("收藏");
    doc.set_pinned(&leaf.node_id, true, 710).expect("置顶");
    let leaf_node = node(&doc, &leaf.node_id);
    assert!(leaf_node.is_favorite);
    assert!(leaf_node.is_pinned);
    assert_eq!(leaf_node.edited_at, 710);
    // 别的节点不受影响
    assert!(!node(&doc, &parent.node_id).is_favorite);
}

#[test]
fn delete_and_restore_follow_event_semantics() {
    let doc = doc(1);
    let seed = doc.create_note("会被删掉", 100).expect("建笔记");

    let first_event = doc.delete_note(&seed.node_id, 200).expect("删除").clone();
    let snapshot = doc.snapshot().expect("快照");
    assert_eq!(
        snapshot.nodes.len(),
        1,
        "删除**不**移除树节点（不用 LoroTree::delete）"
    );
    let node_state = &snapshot.nodes[0];
    assert!(node_state.deleted, "存在未确认删除事件 → 有效删除");
    assert_eq!(node_state.delete_events, vec![first_event.clone()]);
    assert!(node_state.confirmed_events.is_empty());

    let confirmed = doc.restore_note(&seed.node_id, 300).expect("恢复");
    assert_eq!(confirmed, 1, "恢复只确认它见过的删除");
    assert!(!node(&doc, &seed.node_id).deleted);
    assert_eq!(
        node(&doc, &seed.node_id).confirmed_events,
        vec![first_event.clone()]
    );

    // 再删一次：新事件不在确认集合里 → 又变回删除
    let second_event = doc.delete_note(&seed.node_id, 400).expect("再删");
    let node_state = node(&doc, &seed.node_id);
    assert!(node_state.deleted, "恢复之后的新删除仍然获胜");
    assert_eq!(
        node_state.delete_events,
        vec![first_event.clone(), second_event.clone()]
    );
    assert_eq!(node_state.confirmed_events.len(), 1, "新事件尚未被确认");

    // 再恢复：把剩下的也确认掉
    assert_eq!(doc.restore_note(&seed.node_id, 500).expect("再恢复"), 1);
    assert!(!node(&doc, &seed.node_id).deleted);
}

#[test]
fn concurrent_moves_converge_to_same_tree() {
    let a = doc(1);
    let n1 = a.create_note("一", 100).expect("建一");
    let n2 = a.create_note("二", 110).expect("建二");
    let n3 = a.create_note("三", 120).expect("建三");

    let b = doc(2);
    b.import(&a.export_all_updates().expect("导出基准"))
        .expect("b 导入基准");

    // 并发移动：a 把 n3 挂到 n1 下；b 把 n1 挂到 n2 下
    a.move_node(&n3.node_id, Some(&n1.node_id), 200)
        .expect("a 移动");
    b.move_node(&n1.node_id, Some(&n2.node_id), 210)
        .expect("b 移动");

    sync(&a, &b);

    let snapshot_a = a.snapshot().expect("a 快照");
    let snapshot_b = b.snapshot().expect("b 快照");
    assert_eq!(snapshot_a, snapshot_b, "两副本并发移动后必须收敛到同一棵树");
    assert_eq!(snapshot_a.nodes.len(), 3, "并发移动不丢节点");
    assert_acyclic(&snapshot_a);

    let by_id: HashMap<&str, &NodeSnapshot> = snapshot_a
        .nodes
        .iter()
        .map(|node| (node.node_id.as_str(), node))
        .collect();
    assert_eq!(
        by_id[n3.node_id.as_str()].parent.as_deref(),
        Some(n1.node_id.as_str())
    );
    assert_eq!(
        by_id[n1.node_id.as_str()].parent.as_deref(),
        Some(n2.node_id.as_str())
    );
    assert_eq!(by_id[n2.node_id.as_str()].parent, None);
}

#[test]
fn concurrent_delete_and_restore_keeps_delete_winning() {
    let a = doc(1);
    let seed = a.create_note("并发", 100).expect("建笔记");
    let b = doc(2);
    b.import(&a.export_all_updates().expect("导出基准"))
        .expect("b 导入基准");

    // a 删除；b 在没见到删除事件的情况下恢复（只能确认空集合）
    let event = a.delete_note(&seed.node_id, 200).expect("a 删除");
    let confirmed = b.restore_note(&seed.node_id, 210).expect("b 恢复");
    assert_eq!(confirmed, 0, "b 没见过任何删除事件");

    sync(&a, &b);

    let snapshot_a = a.snapshot().expect("a 快照");
    let snapshot_b = b.snapshot().expect("b 快照");
    assert_eq!(snapshot_a, snapshot_b, "并发「删除 × 恢复」必须收敛");
    assert!(
        snapshot_a.nodes[0].deleted,
        "未确认的并发删除获胜：ADR §3.3"
    );
    assert_eq!(snapshot_a.nodes[0].delete_events, vec![event]);
    assert!(snapshot_a.nodes[0].confirmed_events.is_empty());

    // 见到事件后再恢复才真正生效
    assert_eq!(a.restore_note(&seed.node_id, 300).expect("a 恢复"), 1);
    sync(&a, &b);
    assert!(!a.snapshot().expect("a").nodes[0].deleted);
    assert!(!b.snapshot().expect("b").nodes[0].deleted);
}

#[test]
fn concurrent_deletes_from_two_peers_need_all_acks() {
    let a = doc(1);
    let seed = a.create_note("双重删除", 100).expect("建笔记");
    let b = doc(2);
    b.import(&a.export_all_updates().expect("导出基准"))
        .expect("b 导入基准");

    let event_a = a.delete_note(&seed.node_id, 200).expect("a 删除");
    let event_b = b.delete_note(&seed.node_id, 210).expect("b 删除");
    sync(&a, &b);

    let node_state = node(&a, &seed.node_id);
    assert!(node_state.deleted);
    assert_eq!(
        node_state.delete_events.len(),
        2,
        "两个并发删除事件都要保留"
    );
    assert!(node_state.delete_events.contains(&event_a));
    assert!(node_state.delete_events.contains(&event_b));

    // 恢复会确认它已经看到的两个事件
    assert_eq!(a.restore_note(&seed.node_id, 300).expect("a 恢复"), 2);
    sync(&a, &b);
    assert!(!node(&b, &seed.node_id).deleted);

    // 恢复之后的并发新删除仍然获胜
    let event_c = b.delete_note(&seed.node_id, 400).expect("b 再删");
    sync(&a, &b);
    let node_state = node(&a, &seed.node_id);
    assert!(node_state.deleted, "恢复后的新删除未被确认");
    assert!(node_state.delete_events.contains(&event_c));
}

#[test]
fn concurrent_tag_adds_union_without_duplicates() {
    let a = doc(1);
    let seed = a.create_note("标签并发", 100).expect("建笔记");
    let b = doc(2);
    b.import(&a.export_all_updates().expect("导出基准"))
        .expect("b 导入基准");

    a.add_tag(&seed.node_id, "工作", 200).expect("a 加标签");
    b.add_tag(&seed.node_id, "生活", 210).expect("b 加标签");
    sync(&a, &b);

    let snapshot = a.snapshot().expect("快照");
    assert_eq!(
        snapshot,
        b.snapshot().expect("b 快照"),
        "并发加标签必须收敛"
    );
    assert_eq!(snapshot.nodes[0].tags.len(), 2, "取并集且无重复");
    assert_eq!(snapshot.tags.len(), 2, "字典里两项");

    // 两边加同一个标签：集合语义下仍只有一项
    a.add_tag(&seed.node_id, "共同", 300).expect("a 加共同");
    b.add_tag(&seed.node_id, "共同", 310).expect("b 加共同");
    sync(&a, &b);
    assert_eq!(a.snapshot().expect("快照"), b.snapshot().expect("快照"));
    assert_eq!(a.snapshot().expect("快照").nodes[0].tags.len(), 3);
}

#[test]
fn snapshot_is_deterministic_across_replays() {
    let a = doc(1);
    let seed = a.create_note("确定性", 100).expect("建笔记");
    a.rename(&seed.node_id, "改名后", 200).expect("改名");
    a.add_tag(&seed.node_id, "标签", 300).expect("加标签");
    let bytes = a.export_all_updates().expect("导出");

    // 同一份 update 重放两次，快照必须完全一致（投影可幂等重建的前提）
    let first = StructureDoc::restore(9, None, std::slice::from_ref(&bytes)).expect("重放一");
    let second = StructureDoc::restore(9, None, std::slice::from_ref(&bytes)).expect("重放二");
    assert_eq!(
        first.snapshot().expect("快照一"),
        second.snapshot().expect("快照二")
    );
    assert_eq!(
        first.snapshot().expect("快照一"),
        a.snapshot().expect("原快照")
    );

    // 重复导入幂等
    first.import(&bytes).expect("重复导入");
    assert_eq!(
        first.snapshot().expect("快照"),
        second.snapshot().expect("快照")
    );
}
