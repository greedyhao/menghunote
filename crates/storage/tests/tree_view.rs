//! 侧栏树读接口（`list_tree` / `list_notes`）与 `create_folder`。

mod support;

use menghunote_storage::{
    list_notes, list_tree, list_tree_sorted, NodeKind, StructureWriter, TreeEntry, TreeSort,
};
use support::*;

fn open_writer(vault: &menghunote_storage::Vault) -> StructureWriter {
    StructureWriter::open(vault, PEER_A).expect("打开结构写入口")
}

fn shape(entries: &[TreeEntry]) -> Vec<(String, usize, NodeKind)> {
    entries
        .iter()
        .map(|entry| (entry.title.clone(), entry.depth, entry.kind))
        .collect()
}

#[test]
fn empty_vault_lists_nothing() {
    let (_dir, vault) = fresh_vault();
    assert!(list_tree(&vault).expect("空库可读").is_empty());
    assert!(list_notes(&vault).expect("空库可读").is_empty());
}

#[test]
fn notes_and_folders_are_listed_in_preorder_with_depth() {
    let (_dir, vault) = fresh_vault();
    let mut writer = open_writer(&vault);

    // 根级：目录「工作」+ 笔记「根笔记」（按创建顺序排 rank）
    let work = writer.doc().create_folder("工作", 10).expect("建目录");
    let root_note = writer.doc().create_note("根笔记", 11).expect("建笔记");

    // 「工作」下：子目录「子目录」，再下面挂笔记「子笔记」
    let sub = writer.doc().create_folder("子目录", 12).expect("建子目录");
    writer
        .doc()
        .move_node(&sub, Some(&work), 13)
        .expect("子目录挂到工作下");
    let sub_note = writer.doc().create_note("子笔记", 14).expect("建子笔记");
    writer
        .doc()
        .move_node(&sub_note.node_id, Some(&sub), 15)
        .expect("子笔记挂到子目录下");

    writer.commit("test-tree").expect("提交结构");

    let tree = list_tree(&vault).expect("读树");
    assert_eq!(
        shape(&tree),
        vec![
            ("工作".to_owned(), 0, NodeKind::Folder),
            ("子目录".to_owned(), 1, NodeKind::Folder),
            ("子笔记".to_owned(), 2, NodeKind::Note),
            ("根笔记".to_owned(), 0, NodeKind::Note),
        ],
        "必须是先序 DFS：工作 → 子目录 → 子笔记 → 根笔记"
    );

    // 目录：有 node_id / title / parent，没有 note_id / body_doc_id
    let folder_entry = &tree[0];
    assert_eq!(folder_entry.node_id, work);
    assert_eq!(folder_entry.parent_id, None);
    assert_eq!(folder_entry.note_id, None);
    assert_eq!(folder_entry.body_doc_id, None);
    assert!(folder_entry.is_folder() && !folder_entry.is_note());
    assert_eq!(folder_entry.sibling_rank, 0);

    // 子目录的 parent 是「工作」，子笔记的 parent 是「子目录」
    assert_eq!(tree[1].parent_id.as_deref(), Some(work.as_str()));
    assert_eq!(tree[2].parent_id.as_deref(), Some(tree[1].node_id.as_str()));
    assert_eq!(tree[2].note_id.as_deref(), Some(sub_note.note_id.as_str()));
    assert_eq!(
        tree[2].body_doc_id.as_deref(),
        Some(sub_note.body_doc_id.as_str()),
        "笔记要带 body_doc_id（打开编辑器用）"
    );
    // 根级 rank：工作 0、根笔记 1
    assert_eq!(tree[3].node_id, root_note.node_id);
    assert_eq!(tree[3].sibling_rank, 1);
}

#[test]
fn rename_is_reflected_immediately() {
    let (_dir, vault) = fresh_vault();
    let mut writer = open_writer(&vault);
    let folder = writer.doc().create_folder("旧目录名", 10).expect("建目录");
    let note = writer.doc().create_note("旧标题", 11).expect("建笔记");
    writer.commit("create").expect("提交");
    assert_eq!(list_tree(&vault).expect("读树")[0].title, "旧目录名");

    writer
        .doc()
        .rename(&folder, "新目录名", 12)
        .expect("改目录名");
    writer
        .doc()
        .rename(&note.node_id, "新标题", 13)
        .expect("改标题");
    writer.commit("rename").expect("提交改名");

    let tree = list_tree(&vault).expect("读树");
    assert_eq!(tree[0].title, "新目录名");
    assert_eq!(tree[1].title, "新标题");
}

#[test]
fn order_is_stable_across_calls() {
    let (_dir, vault) = fresh_vault();
    let mut writer = open_writer(&vault);
    let folder = writer.doc().create_folder("A", 10).expect("目录");
    let first = writer.doc().create_note("B", 11).expect("笔记");
    let second = writer.doc().create_note("C", 12).expect("笔记");
    writer
        .doc()
        .move_node(&second.node_id, Some(&folder), 13)
        .expect("挂到 A 下");
    writer.commit("create").expect("提交");
    assert!(!first.node_id.is_empty(), "笔记节点要有 node_id");

    let one = list_tree(&vault).expect("第一次");
    let two = list_tree(&vault).expect("第二次");
    assert_eq!(one, two, "同一树状态两次调用结果必须完全一致");
    assert_eq!(
        one.iter()
            .map(|entry| entry.title.as_str())
            .collect::<Vec<_>>(),
        vec!["A", "C", "B"],
        "rank 决定顺序，与创建先后无关（C 在 A 下 depth 1）"
    );
    assert_eq!(one[1].title, "C");
    assert_eq!(one[1].depth, 1);
    assert_eq!(one[2].title, "B");
    assert_eq!(one[2].depth, 0);
}

#[test]
fn list_notes_filters_folders_but_keeps_real_depth() {
    let (_dir, vault) = fresh_vault();
    let mut writer = open_writer(&vault);
    let folder = writer.doc().create_folder("目录", 10).expect("目录");
    let inner = writer.doc().create_note("里面的笔记", 11).expect("笔记");
    writer
        .doc()
        .move_node(&inner.node_id, Some(&folder), 12)
        .expect("挂到目录下");
    let root = writer.doc().create_note("根笔记", 13).expect("笔记");
    writer.commit("create").expect("提交");

    let notes = list_notes(&vault).expect("读笔记");
    assert_eq!(notes.len(), 2);
    assert!(notes.iter().all(|entry| entry.is_note()));
    assert_eq!(notes[0].title, "里面的笔记");
    assert_eq!(notes[0].depth, 1, "过滤目录后 depth 仍是真实深度");
    assert_eq!(notes[1].title, "根笔记");
    assert_eq!(notes[1].node_id, root.node_id);
    assert_eq!(notes[1].depth, 0);
}

#[test]
fn deleted_note_disappears_from_both_views() {
    let (_dir, vault) = fresh_vault();
    let mut writer = open_writer(&vault);
    let note = writer.doc().create_note("会被删的", 10).expect("笔记");
    let keep = writer.doc().create_note("留下的", 11).expect("笔记");
    writer.commit("create").expect("提交");
    assert_eq!(list_tree(&vault).expect("读树").len(), 2);

    writer
        .doc()
        .delete_note(&note.node_id, 12)
        .expect("删除笔记");
    writer.commit("delete").expect("提交删除");

    let tree = list_tree(&vault).expect("读树");
    assert_eq!(tree.len(), 1, "被删节点不返回");
    assert_eq!(tree[0].node_id, keep.node_id);
    assert_eq!(list_notes(&vault).expect("读笔记").len(), 1);
}

#[test]
fn folder_without_commit_is_not_listed_until_projected() {
    // 读的是投影表：未 commit（未投影）的结构改动不应出现——这是「读投影」的代价，写明以免误解
    let (_dir, vault) = fresh_vault();
    let mut writer = open_writer(&vault);
    writer.doc().create_folder("待提交", 10).expect("目录");
    assert!(
        list_tree(&vault).expect("读树").is_empty(),
        "未投影前读不到"
    );

    writer.commit("create").expect("提交");
    assert_eq!(list_tree(&vault).expect("读树")[0].title, "待提交");
}

// ---------------------------------------------------------------------------
// 侧栏排序（`list_tree_sorted`）：目录优先 + 六种排序键
// ---------------------------------------------------------------------------

fn titles(entries: &[TreeEntry]) -> Vec<&str> {
    entries.iter().map(|entry| entry.title.as_str()).collect()
}

/// 根级：目录 F1、笔记 beta、目录 F2、笔记 Alpha；F1 下：目录 Sub、笔记 笔记乙、笔记 笔记甲。
fn sorted_sample(vault: &menghunote_storage::Vault) {
    let mut writer = open_writer(vault);
    writer.doc().create_note("beta", 100).expect("笔记");
    let f1 = writer.doc().create_folder("F1", 101).expect("目录");
    writer.doc().create_note("Alpha", 102).expect("笔记");
    writer.doc().create_folder("F2", 103).expect("目录");
    let sub = writer.doc().create_folder("Sub", 104).expect("子目录");
    writer
        .doc()
        .move_node(&sub, Some(&f1), 105)
        .expect("挂子目录");
    let yi = writer.doc().create_note("笔记乙", 106).expect("笔记");
    writer
        .doc()
        .move_node(&yi.node_id, Some(&f1), 107)
        .expect("挂笔记乙");
    let jia = writer.doc().create_note("笔记甲", 108).expect("笔记");
    writer
        .doc()
        .move_node(&jia.node_id, Some(&f1), 109)
        .expect("挂笔记甲");
    writer.commit("sorted-sample").expect("提交结构");
}

#[test]
fn sorted_view_puts_folders_before_notes_at_every_level() {
    let (_dir, vault) = fresh_vault();
    sorted_sample(&vault);

    let tree = list_tree_sorted(&vault, TreeSort::NameAsc).expect("读树");
    assert_eq!(
        titles(&tree),
        vec!["F1", "Sub", "笔记乙", "笔记甲", "F2", "Alpha", "beta"],
        "每个层级目录都在笔记前；组内按文件名 A-Z"
    );
    // 深度/父子结构保留：Sub 在 F1 之后 depth 1，笔记在 Sub 之后仍 depth 1
    assert_eq!(
        tree.iter().map(|entry| entry.depth).collect::<Vec<_>>(),
        vec![0, 1, 1, 1, 0, 0, 0]
    );
    assert_eq!(tree[2].parent_id.as_deref(), Some(tree[0].node_id.as_str()));
    assert_eq!(tree[1].parent_id.as_deref(), Some(tree[0].node_id.as_str()));

    // 与 rank 顺序不同：rank 顺序是创建顺序（beta、F1、Alpha、F2 …）
    let ranked = list_tree(&vault).expect("读 rank 树");
    assert_eq!(
        titles(&ranked),
        vec!["beta", "F1", "Sub", "笔记乙", "笔记甲", "Alpha", "F2"],
        "list_tree 的公共语义保持不变（rank 顺序）"
    );
}

#[test]
fn name_sort_is_case_insensitive_and_chinese_stable() {
    let (_dir, vault) = fresh_vault();
    let mut writer = open_writer(&vault);
    for (index, title) in ["beta", "Alpha", "alpha", "笔记乙", "笔记甲"]
        .iter()
        .enumerate()
    {
        writer
            .doc()
            .create_note(title, 100 + index as i64)
            .expect("笔记");
    }
    writer.commit("names").expect("提交");

    let asc = list_tree_sorted(&vault, TreeSort::NameAsc).expect("读树");
    assert_eq!(
        titles(&asc),
        vec!["Alpha", "alpha", "beta", "笔记乙", "笔记甲"],
        "大小写不敏感（alpha < beta），大小写全同再按原串；中文按码点稳定"
    );
    let desc = list_tree_sorted(&vault, TreeSort::NameDesc).expect("读树");
    assert_eq!(
        titles(&desc),
        vec!["笔记甲", "笔记乙", "beta", "alpha", "Alpha"],
        "Z-A 是 A-Z 的严格逆序（末位 node_id 兜底仍确定）"
    );

    // 两次调用完全一致（纯函数式读取，不依赖 HashMap 迭代顺序）
    assert_eq!(
        list_tree_sorted(&vault, TreeSort::NameAsc).expect("读树"),
        asc
    );
}

#[test]
fn six_sort_keys_use_joined_timestamps() {
    let (_dir, vault) = fresh_vault();
    let mut writer = open_writer(&vault);
    // 创建时间 A(10) < B(20) < C(30)；随后 A 改到 updated=15、B 改到 updated=25。
    // → 创建序 A,B,C；修改序（新→旧）C,B,A；两者方向一致但数值可区分。
    let a = writer.doc().create_note("A", 10).expect("A");
    let b = writer.doc().create_note("B", 20).expect("B");
    let c = writer.doc().create_note("C", 30).expect("C");
    writer.doc().rename(&a.node_id, "A", 15).expect("A 改名");
    writer.doc().rename(&b.node_id, "B", 25).expect("B 改名");
    // C 不再编辑：updated_at = created_at = 30
    assert!(!c.node_id.is_empty());
    writer.commit("times").expect("提交");

    let tree = list_tree(&vault).expect("读 rank 树");
    let find = |title: &str| {
        tree.iter()
            .find(|entry| entry.title == title)
            .unwrap_or_else(|| panic!("缺少 {title}"))
    };
    assert_eq!(find("A").created_at, Some(10));
    assert_eq!(find("A").updated_at, Some(15), "updated_at 跟着改名");
    assert_eq!(find("B").created_at, Some(20));
    assert_eq!(find("B").updated_at, Some(25));
    assert_eq!(find("C").created_at, Some(30));
    assert_eq!(find("C").updated_at, Some(30));

    assert_eq!(
        titles(&list_tree_sorted(&vault, TreeSort::NameAsc).expect("读树")),
        vec!["A", "B", "C"]
    );
    assert_eq!(
        titles(&list_tree_sorted(&vault, TreeSort::NameDesc).expect("读树")),
        vec!["C", "B", "A"]
    );
    assert_eq!(
        titles(&list_tree_sorted(&vault, TreeSort::UpdatedNewest).expect("读树")),
        vec!["C", "B", "A"],
        "修改时间 新→旧（30 / 25 / 15）"
    );
    assert_eq!(
        titles(&list_tree_sorted(&vault, TreeSort::UpdatedOldest).expect("读树")),
        vec!["A", "B", "C"],
        "修改时间 旧→新"
    );
    assert_eq!(
        titles(&list_tree_sorted(&vault, TreeSort::CreatedNewest).expect("读树")),
        vec!["C", "B", "A"],
        "创建时间 新→旧（30 / 20 / 10）"
    );
    assert_eq!(
        titles(&list_tree_sorted(&vault, TreeSort::CreatedOldest).expect("读树")),
        vec!["A", "B", "C"],
        "创建时间 旧→新"
    );
}

#[test]
fn sorted_view_keeps_folders_group_and_depths_with_time_keys() {
    let (_dir, vault) = fresh_vault();
    sorted_sample(&vault);
    for sort in TreeSort::ALL {
        let tree = list_tree_sorted(&vault, sort).expect("读树");
        assert_eq!(tree.len(), 7, "{sort:?} 不能丢节点");
        // 每个目录后面紧跟它的全部后代（折叠依赖的连续区间性质）
        for (index, entry) in tree.iter().enumerate() {
            if entry.is_folder() {
                let next_is_child = tree
                    .get(index + 1)
                    .is_some_and(|next| next.depth == entry.depth + 1);
                if entry.title == "F1" {
                    assert!(next_is_child, "{sort:?}：F1 后必须紧跟子项");
                }
            }
        }
        // 目录永远在笔记前：每个层级只看「同一 parent」的兄弟序列
        for parent in [None, Some(tree[0].node_id.as_str())] {
            let siblings: Vec<&TreeEntry> = tree
                .iter()
                .filter(|entry| entry.parent_id.as_deref() == parent)
                .collect();
            let first_note = siblings.iter().position(|entry| entry.is_note());
            let last_folder = siblings.iter().rposition(|entry| entry.is_folder());
            if let (Some(first_note), Some(last_folder)) = (first_note, last_folder) {
                assert!(
                    last_folder < first_note,
                    "{sort:?}：目录必须全在笔记前：{siblings:?}"
                );
            }
        }
    }
}

#[test]
fn sorted_view_skips_deleted_and_reports_folder_timestamps_as_none() {
    let (_dir, vault) = fresh_vault();
    let mut writer = open_writer(&vault);
    let folder = writer.doc().create_folder("目录", 10).expect("目录");
    let gone = writer.doc().create_note("会被删", 11).expect("笔记");
    writer.doc().create_note("留下", 12).expect("笔记");
    writer.commit("create").expect("提交");
    writer
        .doc()
        .delete_note(&gone.node_id, 13)
        .expect("删除笔记");
    writer.commit("delete").expect("提交删除");

    let tree = list_tree_sorted(&vault, TreeSort::UpdatedNewest).expect("读树");
    assert_eq!(titles(&tree), vec!["目录", "留下"]);
    assert_eq!(tree[0].node_id, folder);
    assert!(tree[0].is_folder());
    assert_eq!(tree[0].created_at, None, "目录没有 notes 行 → 无时间戳");
    assert_eq!(tree[0].updated_at, None);
    assert!(tree[1].is_note());
    assert!(tree[1].created_at.is_some() && tree[1].updated_at.is_some());
}

#[test]
fn tree_sort_round_trips_through_stable_keys() {
    for sort in TreeSort::ALL {
        assert_eq!(sort.as_str(), sort.as_str());
        assert_eq!(TreeSort::parse(sort.as_str()), Some(sort));
    }
    assert_eq!(TreeSort::default(), TreeSort::NameAsc);
    assert_eq!(TreeSort::parse("name_asc"), Some(TreeSort::NameAsc));
    assert_eq!(TreeSort::parse("  name_desc "), Some(TreeSort::NameDesc));
    assert_eq!(TreeSort::parse("bogus"), None);
    assert_eq!(TreeSort::parse(""), None);
}
