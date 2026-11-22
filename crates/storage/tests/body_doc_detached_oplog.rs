//! 任务 2 P0：detached oplog 测试矩阵（只加测试，不改生产代码）。
//!
//! `BodyDoc::import_oplog_only` / `pending_update_from_oplog` 是 `pub(crate)`，生产里唯一
//! 调用方是 baseline apply 的 `existing_local_extra` 分支（见
//! `crates/storage/src/baseline_apply.rs`）。本文件不放大可见性、不改生产代码，而是驱动
//! 该分支真正落库的 `baseline_merge` 行来验证语义：
//!
//! 1. **detached 路径**：`BodyDoc::restore(基线快照)` → `detach` → `import(本地差集)` →
//!    `pending_update_from_oplog(本地 VV)`。产物就是 `loro_updates` 里
//!    `batch_id == BASELINE_MERGE_BATCH` 的那一行（payload / end_frontier / end_vv）。
//! 2. **attached 参考**：`BodyDoc::restore(基线快照) + import(本地差集)`——同一条本地
//!    增量，改成 attached 导入。
//! 3. **attached 重放**：`BodyDoc::restore(None, 库里全部 update)` 与生产读取路径
//!    `load_body_doc`。
//!
//! 每个用例都断言：detached 的 `end_frontier` 与 attached 的 frontier 逐字节相等，
//! `end_vv` 与 attached 的 VV 语义相等（`VersionVector::encode` 的字节顺序取决于
//! import 顺序，不是规范形式，见 [`canonical_vv`]）；三份 attached 结果的**正文逐字节
//! 相同**；本地与基线内容不丢、不重复。
//!
//! LoroTree 用例：`BodyDoc` 只暴露正文 text，`StructureDoc` 不暴露 detach，因此按任务
//! 约定用 `loro` 直接构造 `LoroDoc` / `LoroTree`，验证 `import_oplog_only` 依赖的同一条
//! CRDT 不变量：detach 后 import 只写 oplog；从 oplog 导出的差集 attached 重放后，
//! frontier / VV 与树的可见形状（遍历结果）都与 attached 合并完全一致。

mod support;

use loro::{ExportMode, LoroDoc, LoroTree, LoroValue, TreeID, VersionVector};
use support::*;

use menghunote_storage::{
    apply_baseline_batch, begin_baseline_apply, doc_updates_since, finish_baseline_apply,
    load_body_doc, BaselineApplyFinish, BaselineExpectation, BodyDoc, StoredLoroUpdate, Vault,
    BASELINE_MERGE_BATCH,
};

/// 本地设备（attached 侧）。
const A_PEER: u64 = PEER_A;
/// 远端基线导出方（与本地不同的 peer）。
const B_PEER: u64 = PEER_B;

// ---------------------------------------------------------------------------
// 正文（LoroText）用例的脚手架
// ---------------------------------------------------------------------------

/// 在内存里构造一篇正文（不落库），用作「远端基线」。
fn memory_doc(doc_id: &str, peer: u64, text: &str) -> BodyDoc {
    let doc = BodyDoc::create(doc_id, peer).expect("创建内存文档");
    if !text.is_empty() {
        doc.insert_scalars(0, text).expect("写远端基线正文");
    }
    doc
}

/// begin 一份「只含这一篇文档」的 baseline staging。
fn begin_apply(vault: &Vault, doc_id: &str, baseline_s: u64, cursor: &str) {
    let expected = BaselineExpectation::new(
        std::iter::once(doc_id.to_owned()),
        std::iter::empty::<[u8; 32]>(),
    );
    let checkpoint_id: [u8; 16] = [baseline_s as u8; 16];
    begin_baseline_apply(vault, &checkpoint_id, baseline_s, cursor, &expected)
        .expect("begin baseline apply");
}

/// 快照蕴含的 VV（生产 `baseline_vv` 的来源：restore snapshot 后取 vv）。
fn snapshot_vv(snapshot: &[u8]) -> VersionVector {
    BodyDoc::restore("vv-probe", B_PEER, Some(snapshot), &[])
        .expect("基线快照可恢复")
        .vv()
}

/// VV 的规范表示：`VersionVector::encode` 的字节顺序取决于 import 顺序（不是规范形式），
/// 所以按 peer 排序后比较「peer -> counter」集合；语义相等即等价。
fn canonical_vv(bytes: &[u8]) -> Vec<(u64, i32)> {
    let vv = VersionVector::decode(bytes).expect("VV 可解码");
    let mut entries: Vec<(u64, i32)> = vv.iter().map(|(peer, counter)| (*peer, *counter)).collect();
    entries.sort_unstable();
    entries
}

/// `loro_updates` 里 baseline merge 追加的那一行。
struct DetachedMerge {
    /// payload 由 `pending_update_from_oplog` 导出，end_frontier / end_vv 取 **oplog** 版本。
    merged: StoredLoroUpdate,
    /// 本地相对基线 VV 的差集（与生产 detached 分支 import 的字节同源）。
    local_delta: Vec<u8>,
}

/// 驱动生产 `existing_local_extra`（detached）分支，并取回它落库的差集。
fn merge_detached(
    vault: &Vault,
    doc_id: &str,
    snapshot: &[u8],
    local: &BodyDoc,
    baseline_s: u64,
    cursor: &str,
) -> DetachedMerge {
    let local_delta = local
        .pending_update(&snapshot_vv(snapshot))
        .expect("导出本地差集")
        .payload;
    assert!(
        !local_delta.is_empty(),
        "本地必须在基线之外还有 op，否则生产走 attached 分支，测不到 detached"
    );
    begin_apply(vault, doc_id, baseline_s, cursor);
    apply_baseline_batch(vault, &[(doc_id.to_owned(), snapshot.to_vec())], &[])
        .expect("应用基线快照");
    let merged = doc_updates_since(vault, doc_id, 0)
        .expect("读 loro_updates")
        .into_iter()
        .rfind(|update| update.batch_id == BASELINE_MERGE_BATCH)
        .expect("detached 分支必须落一条 baseline_merge 差集");
    assert!(!merged.payload.is_empty(), "detached 导出的差集不能为空");

    // 分支守卫：detached 合并后的 oplog frontier 必须同时包含本地与基线两个 peer。
    // 若生产误走 attached 分支，这里只会是基线 state 的单 peer frontier。
    let frontier = loro::Frontiers::decode(&merged.end_frontier).expect("frontier 可解码");
    let mut peers: Vec<u64> = frontier.iter().map(|id| id.peer).collect();
    peers.sort_unstable();
    peers.dedup();
    assert_eq!(
        peers,
        vec![A_PEER, B_PEER],
        "detached 分支未生效？合并后的 oplog frontier 必须同时包含两个 peer"
    );

    DetachedMerge {
        merged,
        local_delta,
    }
}

/// 断言 detached 结果与 attached 路径完全等价，返回合并后的正文。
///
/// attached 侧有三条证据链，全部要与 detached 的 frontier 逐字节一致、VV 语义一致：
/// 1. 基线副本 attached 导入同一条本地差集；
/// 2. 库里全部 update 重新 restore（attached 重放）；
/// 3. 生产读取入口 `load_body_doc`。
fn assert_detached_matches_attached(
    vault: &Vault,
    doc_id: &str,
    snapshot: &[u8],
    parts: &DetachedMerge,
) -> String {
    // 1) attached 参考：基线副本 + attached 导入本地差集。
    let attached =
        BodyDoc::restore(doc_id, A_PEER, Some(snapshot), &[]).expect("attached 基线副本");
    attached
        .import(&parts.local_delta)
        .expect("attached 导入本地差集");
    let attached_text = attached.text();
    assert_eq!(
        canonical_vv(&attached.vv().encode()),
        canonical_vv(&parts.merged.end_vv),
        "detached oplog VV 与 attached 合并 VV 不一致"
    );
    assert_eq!(
        attached.frontiers().encode(),
        parts.merged.end_frontier,
        "detached oplog frontier 与 attached 合并 frontier 不一致"
    );

    // 2) attached 重放：库里全部 update（本地 update + detached 导出的差集）。
    let payloads: Vec<Vec<u8>> = doc_updates_since(vault, doc_id, 0)
        .expect("读全部 update")
        .into_iter()
        .map(|update| update.payload)
        .collect();
    let replayed = BodyDoc::restore(doc_id, A_PEER, None, &payloads).expect("attached 重放");
    assert_eq!(
        replayed.text(),
        attached_text,
        "attached 重放文本与 attached 合并文本不是逐字节相同"
    );
    assert_eq!(
        canonical_vv(&replayed.vv().encode()),
        canonical_vv(&parts.merged.end_vv)
    );
    assert_eq!(replayed.frontiers().encode(), parts.merged.end_frontier);

    // 3) 生产读取路径：文本、frontier、VV 必须与 attached 参考完全一致。
    let (loaded, head) = load_body_doc(vault, doc_id, A_PEER, None).expect("生产重放");
    assert_eq!(loaded.text(), attached_text, "load_body_doc 文本不一致");
    assert_eq!(
        canonical_vv(&loaded.vv().encode()),
        canonical_vv(&parts.merged.end_vv)
    );
    assert_eq!(loaded.frontiers().encode(), parts.merged.end_frontier);
    assert_eq!(
        head.head_frontier, parts.merged.end_frontier,
        "head_frontier 必须来自 detached 合并后的 oplog frontier"
    );
    assert_eq!(
        canonical_vv(&head.head_vv),
        canonical_vv(&parts.merged.end_vv),
        "head_vv 必须来自 detached 合并后的 oplog VV"
    );

    attached_text
}

// ---------------------------------------------------------------------------
// 1. 两个 peer 并发编辑同一 LoroText
// ---------------------------------------------------------------------------

#[test]
fn concurrent_two_peer_text_inserts_match_attached_byte_for_byte() {
    let (_dir, vault) = fresh_vault();
    // 本地 peer A：独立文档，与基线完全不共享历史（最纯粹的并发同位置插入）。
    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "LOCAL").expect("本地编辑");
    commit_now(&vault, &local, "local-batch");

    // 远端 peer B：导出基线快照。
    let remote = memory_doc("doc-1", B_PEER, "REMOTE");
    let snapshot = remote.export_snapshot().expect("基线快照");

    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 5, "cursor-1");
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(
        text.matches("LOCAL").count(),
        1,
        "本地文本必须恰好出现一次：{text:?}"
    );
    assert_eq!(
        text.matches("REMOTE").count(),
        1,
        "基线文本必须恰好出现一次：{text:?}"
    );
    assert!(
        text.is_ascii(),
        "纯 ASCII 合并后不得引入非 ASCII 字节：{text:?}"
    );
    assert_eq!(
        text.len(),
        "LOCAL".len() + "REMOTE".len(),
        "字节数必须是两段 ASCII 之和：{text:?}"
    );
    assert_eq!(text.chars().count(), 11);
}

#[test]
fn local_op_depending_on_baseline_matches_attached_after_detached_merge() {
    let (_dir, vault) = fresh_vault();
    // 远端：先导出 base，再在本地收不到的情况下追加 -r2（新基线是原来 + 这一笔）。
    let remote = memory_doc("doc-1", B_PEER, "base");
    let partial = remote.export_snapshot().expect("base 快照");
    remote.insert_scalars(4, "-r2").expect("远端第二笔");
    let full = remote.export_snapshot().expect("full 快照");

    // 本地：先收到 base，再在 base 之上编辑——本地 op 因果依赖基线 op。
    let _ = seed_note(&vault, "note-1", "doc-1");
    let local = BodyDoc::restore("doc-1", A_PEER, Some(&partial), &[]).expect("本地副本");
    local.insert_scalars(4, "-local").expect("本地并发编辑");
    commit_now(&vault, &local, "local-batch");

    let parts = merge_detached(&vault, "doc-1", &full, &local, 6, "cursor-2");
    let text = assert_detached_matches_attached(&vault, "doc-1", &full, &parts);

    assert_eq!(
        text.matches("-local").count(),
        1,
        "依赖基线的本地 op 必须恰好一次：{text:?}"
    );
    assert_eq!(
        text.matches("-r2").count(),
        1,
        "远端并发 op 必须恰好一次：{text:?}"
    );
    assert_eq!(
        text.matches("base").count(),
        1,
        "基线文本必须恰好一次：{text:?}"
    );
    assert_eq!(
        text.len(),
        "base".len() + "-local".len() + "-r2".len(),
        "不得有重复或残留字符：{text:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. LoroTree create / move / delete
// ---------------------------------------------------------------------------

/// 新建一个 LoroDoc（未启用任何容器配置）。
fn tree_doc(peer: u64) -> LoroDoc {
    let doc = LoroDoc::new();
    doc.set_peer_id(peer).expect("设置 peer id");
    doc
}

/// 取树容器并启用 fractional index（与 `StructureDoc::create/restore` 一致）。
fn editable_tree(doc: &LoroDoc) -> LoroTree {
    let tree = doc.get_tree("tree");
    tree.enable_fractional_index(0);
    tree
}

fn set_title(tree: &LoroTree, node: TreeID, title: &str) {
    tree.get_meta(node)
        .expect("节点 meta 可写")
        .insert("title", title)
        .expect("写标题");
}

fn title_of(tree: &LoroTree, node: TreeID) -> String {
    let meta = tree.get_meta(node).expect("节点 meta 可读");
    match meta.get_deep_value() {
        LoroValue::Map(entries) => entries
            .get("title")
            .and_then(|value| match value {
                LoroValue::String(value) => Some(value.to_string()),
                _ => None,
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// 树的可读形状：深度优先的「标题（含缩进）」，根顺序也参与比较。
fn shape(doc: &LoroDoc) -> Vec<String> {
    let tree = doc.get_tree("tree");
    let mut out = Vec::new();
    walk_shape(&tree, None, 0, &mut out);
    out
}

fn walk_shape(tree: &LoroTree, parent: Option<TreeID>, depth: usize, out: &mut Vec<String>) {
    let Some(children) = tree.children(parent) else {
        return;
    };
    for child in children {
        out.push(format!("{}{}", "  ".repeat(depth), title_of(tree, child)));
        walk_shape(tree, Some(child), depth + 1, out);
    }
}

/// 节点 ID + 父节点 ID（连同深度），用于断言两副本是同一棵树而不是「同形状」。
fn parent_ids(doc: &LoroDoc) -> Vec<String> {
    let tree = doc.get_tree("tree");
    let mut out = Vec::new();
    walk_ids(&tree, None, 0, &mut out);
    out
}

fn walk_ids(tree: &LoroTree, parent: Option<TreeID>, depth: usize, out: &mut Vec<String>) {
    let Some(children) = tree.children(parent) else {
        return;
    };
    for child in children {
        let parent_label = parent
            .map(|parent| parent.to_string())
            .unwrap_or_else(|| "<root>".to_owned());
        out.push(format!("{parent_label}|{child}|{depth}"));
        walk_ids(tree, Some(child), depth + 1, out);
    }
}

/// 在树上跑一遍 detached / attached 三条路径，返回 attached 参考文档。
///
/// - `snapshot`：本轮要应用的远端基线快照（peer B 导出）；
/// - `local_delta`：本地相对该快照 VV 的差集；
/// - `local_vv`：本地副本的 oplog VV（detached 导出的基准）；
/// - `local_snapshot`：本地副本完整快照（attached 重放的起点）。
fn assert_tree_paths_agree(
    peer: u64,
    snapshot: &[u8],
    local_delta: &[u8],
    local_vv: &VersionVector,
    local_snapshot: &[u8],
) -> LoroDoc {
    // 1) attached 参考：基线副本 attached 导入同一条本地差集。
    let attached = tree_doc(peer);
    attached.import(snapshot).expect("attached 基线快照");
    attached.import(local_delta).expect("attached 合并本地差集");

    // 2) detached：先 detach，只把增量写进 oplog，再从 oplog 导出差集。
    let detached = tree_doc(peer);
    detached.import(snapshot).expect("detached 基线快照");
    editable_tree(&detached);
    detached.detach();
    assert!(detached.is_detached());
    detached.import(local_delta).expect("detached import");
    assert!(
        detached.is_detached(),
        "import 不得把 detached 文档翻回 attached"
    );
    assert_eq!(
        detached.oplog_frontiers().encode(),
        attached.state_frontiers().encode(),
        "detached 合并 frontier 与 attached 合并 frontier 不一致"
    );
    assert_eq!(
        detached.oplog_frontiers().encode(),
        attached.oplog_frontiers().encode(),
        "detached oplog frontier 与 attached oplog frontier 不一致"
    );
    assert_eq!(
        canonical_vv(&detached.oplog_vv().encode()),
        canonical_vv(&attached.oplog_vv().encode()),
        "detached 合并 VV 与 attached 合并 VV 不一致"
    );
    let delta_from_oplog = detached
        .export(ExportMode::updates(local_vv))
        .expect("从 detached oplog 导出差集");
    assert!(!delta_from_oplog.is_empty(), "本轮 detached 差集必须非空");

    // attach 后读取合并后的树状态（生产从不读 detached 状态，这里只做等价性证据）。
    detached.attach();
    assert!(!detached.is_detached());
    assert_eq!(shape(&detached), shape(&attached), "attach 后树形状不一致");
    assert_eq!(
        parent_ids(&detached),
        parent_ids(&attached),
        "attach 后树父子关系不一致"
    );

    // 3) attached 重放：本地副本 + detached 导出的差集（= 库里本地 update + 合并差集）。
    let replay = tree_doc(peer);
    replay.import(local_snapshot).expect("重放本地副本");
    editable_tree(&replay);
    replay
        .import(&delta_from_oplog)
        .expect("重放 detached 差集");
    assert_eq!(
        replay.state_frontiers().encode(),
        attached.state_frontiers().encode(),
        "attached 重放 frontier 与 attached 合并不一致"
    );
    assert_eq!(
        canonical_vv(&replay.oplog_vv().encode()),
        canonical_vv(&attached.oplog_vv().encode()),
        "attached 重放 VV 与 attached 合并不一致"
    );
    assert_eq!(
        shape(&replay),
        shape(&attached),
        "attached 重放树形状不一致"
    );
    assert_eq!(
        parent_ids(&replay),
        parent_ids(&attached),
        "attached 重放父子关系不一致"
    );

    attached
}

#[test]
fn tree_create_and_move_match_attached_frontier_and_shape() {
    // 第一份基线（peer B）：Folder / Note 1、根级 Note 2。
    let remote = tree_doc(B_PEER);
    let remote_tree = editable_tree(&remote);
    let folder = remote_tree.create(None).expect("创建 Folder");
    let note1 = remote_tree.create(folder).expect("创建 Note 1");
    let note2 = remote_tree.create(None).expect("创建 Note 2");
    set_title(&remote_tree, folder, "Folder");
    set_title(&remote_tree, note1, "Note 1");
    set_title(&remote_tree, note2, "Note 2");
    remote.commit();
    let first = remote.export(ExportMode::Snapshot).expect("第一份基线");

    // 本地（peer A）：在基线上并发 create + move（把 Note 2 重挂到本地新节点下）。
    let local = tree_doc(A_PEER);
    local.import(&first).expect("本地副本");
    let local_tree = editable_tree(&local);
    let local_node = local_tree.create(note1).expect("本地新节点");
    set_title(&local_tree, local_node, "Local");
    local_tree.mov(note2, local_node).expect("本地移动");
    local.commit();
    let local_snapshot = local.export(ExportMode::Snapshot).expect("本地快照");
    let local_vv = local.oplog_vv();

    // 远端在本地编辑期间继续前进：新建根节点 Remote（第二轮 frontier 的来源）。
    let remote_node = remote_tree.create(None).expect("远端新根");
    set_title(&remote_tree, remote_node, "Remote");
    remote.commit();
    let snapshot = remote.export(ExportMode::Snapshot).expect("第二份基线");
    let baseline_vv = remote.oplog_vv();

    let local_delta = local
        .export(ExportMode::updates(&baseline_vv))
        .expect("本地差集");
    assert!(!local_delta.is_empty(), "本地差集不能为空");

    let attached =
        assert_tree_paths_agree(A_PEER, &snapshot, &local_delta, &local_vv, &local_snapshot);

    // 可见形状硬断言：Folder > Note 1 > Local > Note 2，远端新根 Remote 在根级末尾。
    assert_eq!(
        shape(&attached),
        vec![
            "Folder".to_owned(),
            "  Note 1".to_owned(),
            "    Local".to_owned(),
            "      Note 2".to_owned(),
            "Remote".to_owned(),
        ]
    );
}

#[test]
fn tree_delete_and_reparent_match_attached_frontier_and_shape() {
    // 第一份基线（peer B）：Folder / Note 1（本地会删），根级 Note 2、Note 3（本地会重挂）。
    let remote = tree_doc(B_PEER);
    let remote_tree = editable_tree(&remote);
    let folder = remote_tree.create(None).expect("创建 Folder");
    let note1 = remote_tree.create(folder).expect("创建 Note 1");
    let note2 = remote_tree.create(None).expect("创建 Note 2");
    let note3 = remote_tree.create(None).expect("创建 Note 3");
    set_title(&remote_tree, folder, "Folder");
    set_title(&remote_tree, note1, "Note 1");
    set_title(&remote_tree, note2, "Note 2");
    set_title(&remote_tree, note3, "Note 3");
    remote.commit();
    let first = remote.export(ExportMode::Snapshot).expect("第一份基线");

    // 本地（peer A）：并发 delete + create + reparent。
    let local = tree_doc(A_PEER);
    local.import(&first).expect("本地副本");
    let local_tree = editable_tree(&local);
    local_tree.delete(note1).expect("本地删除 Note 1");
    assert!(
        local_tree.is_node_deleted(&note1).expect("删除标记可读"),
        "本地删除后 Note 1 必须是删除态"
    );
    let local_node = local_tree.create(note2).expect("本地新节点");
    set_title(&local_tree, local_node, "Local");
    local_tree.mov(note3, local_node).expect("本地重挂 Note 3");
    local.commit();
    let local_snapshot = local.export(ExportMode::Snapshot).expect("本地快照");
    let local_vv = local.oplog_vv();

    // 远端继续：新建根节点 Remote。
    let remote_node = remote_tree.create(None).expect("远端新根");
    set_title(&remote_tree, remote_node, "Remote");
    remote.commit();
    let snapshot = remote.export(ExportMode::Snapshot).expect("第二份基线");
    let baseline_vv = remote.oplog_vv();

    let local_delta = local
        .export(ExportMode::updates(&baseline_vv))
        .expect("本地差集");
    assert!(!local_delta.is_empty(), "本地差集不能为空");

    let attached =
        assert_tree_paths_agree(A_PEER, &snapshot, &local_delta, &local_vv, &local_snapshot);

    // 可见形状硬断言：Note 1 被删（Folder 无子），Note 2 > Local > Note 3，Remote 在根级末尾。
    assert_eq!(
        shape(&attached),
        vec![
            "Folder".to_owned(),
            "Note 2".to_owned(),
            "  Local".to_owned(),
            "    Note 3".to_owned(),
            "Remote".to_owned(),
        ]
    );
    assert!(
        attached
            .get_tree("tree")
            .is_node_deleted(&note1)
            .expect("删除标记可读"),
        "合并后 Note 1 必须仍是删除态"
    );
}

// ---------------------------------------------------------------------------
// 3. 本地在 baseline 导出之后继续编辑
// ---------------------------------------------------------------------------

#[test]
fn local_edits_after_baseline_export_survive_exactly_once() {
    let (_dir, vault) = fresh_vault();
    let remote = memory_doc("doc-1", B_PEER, "Hello world");
    let first = remote.export_snapshot().expect("第一份基线");

    // 本地在 S0 之后继续写（首尾各一笔），远端也在 S0 之后写（中间一笔）。
    let _ = seed_note(&vault, "note-1", "doc-1");
    let local = BodyDoc::restore("doc-1", A_PEER, Some(&first), &[]).expect("本地副本");
    local.insert_scalars(0, ">> ").expect("本地行首编辑");
    local
        .insert_scalars(local.scalar_len(), "!")
        .expect("本地行尾编辑");
    commit_now(&vault, &local, "local-batch");

    remote.insert_scalars(5, " MID").expect("远端第二笔");
    let full = remote.export_snapshot().expect("第二份基线");

    let parts = merge_detached(&vault, "doc-1", &full, &local, 6, "cursor-2");
    let text = assert_detached_matches_attached(&vault, "doc-1", &full, &parts);

    assert_eq!(
        text, ">> Hello MID world!",
        "并发的本地首尾编辑必须保留且只保留一次：{text:?}"
    );
    assert_eq!(text.matches(">> ").count(), 1);
    assert_eq!(text.matches(" MID").count(), 1);
    assert_eq!(text.matches('!').count(), 1);
    assert_eq!(text.matches("Hello").count(), 1);
    assert_eq!(text.len(), 19);
}

#[test]
fn second_detached_merge_keeps_both_local_writes_once() {
    let (_dir, vault) = fresh_vault();
    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "-L1").expect("本地第一笔");
    commit_now(&vault, &local, "local-round-1");

    let remote = memory_doc("doc-1", B_PEER, "BASE");
    let first = remote.export_snapshot().expect("第一份基线");

    let parts = merge_detached(&vault, "doc-1", &first, &local, 5, "cursor-1");
    let first_text = assert_detached_matches_attached(&vault, "doc-1", &first, &parts);
    assert_eq!(first_text.matches("-L1").count(), 1);
    assert_eq!(first_text.matches("BASE").count(), 1);

    // 第一轮 finish；完成后再 begin 第二轮（第二次 checkpoint 追赶）。
    let finish = finish_baseline_apply(&vault, 5, "cursor-1").expect("finish 第一轮");
    assert!(
        matches!(finish, BaselineApplyFinish::Applied(_)),
        "第一轮 finish 应成功：{finish:?}"
    );

    // 合并态上继续本地写：生产路径就是重放合并结果再编辑。
    let (merged, _head) = load_body_doc(&vault, "doc-1", A_PEER, None).expect("第一轮合并态");
    merged
        .insert_scalars(merged.scalar_len(), "-L2")
        .expect("本地第二笔");
    commit_now(&vault, &merged, "local-round-2");

    // 远端也继续前进：第二轮 detached 差集必须非空（否则测不到 exported 差集）。
    remote.insert_scalars(0, "R2-").expect("远端第二笔");
    let second = remote.export_snapshot().expect("第二份基线");

    let parts = merge_detached(&vault, "doc-1", &second, &merged, 9, "cursor-2");
    let text = assert_detached_matches_attached(&vault, "doc-1", &second, &parts);

    assert_eq!(
        text.matches("-L1").count(),
        1,
        "第一笔本地编辑必须恰好一次：{text:?}"
    );
    assert_eq!(
        text.matches("-L2").count(),
        1,
        "第二笔本地编辑必须恰好一次：{text:?}"
    );
    assert_eq!(text.matches("BASE").count(), 1);
    assert_eq!(text.matches("R2-").count(), 1);
    assert!(text.ends_with("-L2"), "本地追加必须仍在末尾：{text:?}");
    assert_eq!(
        text.len(),
        "BASE".len() + "R2-".len() + "-L1".len() + "-L2".len(),
        "不得有重复或残留字符：{text:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. 边界：空文档 / 单字符 / emoji 与组合字符
// ---------------------------------------------------------------------------

#[test]
fn empty_baseline_text_merges_to_local_text_only() {
    let (_dir, vault) = fresh_vault();
    // 基线：插入再删除——正文为空，但 oplog 非空，走的是 detached 合并而不是「新文档」分支。
    let remote = BodyDoc::create("doc-1", B_PEER).expect("远端基线");
    remote.insert_scalars(0, "x").expect("基线插入");
    remote.delete_scalars(0, 1).expect("基线删除");
    assert_eq!(remote.text(), "");
    let snapshot = remote.export_snapshot().expect("基线快照");

    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "local").expect("本地编辑");
    commit_now(&vault, &local, "local-batch");

    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 7, "cursor-3");
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(text, "local", "空基线不得留下任何可见字符：{text:?}");
    assert_eq!(text.len(), 5);
}

#[test]
fn local_doc_emptied_by_delete_merges_to_baseline_text() {
    let (_dir, vault) = fresh_vault();
    let remote = memory_doc("doc-1", B_PEER, "baseline");
    let snapshot = remote.export_snapshot().expect("基线快照");

    // 本地写过再全部删掉：正文为空但 oplog 非空。
    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "tmp").expect("本地临时文本");
    local.delete_scalars(0, 3).expect("本地清空");
    assert_eq!(local.text(), "");
    commit_now(&vault, &local, "local-batch");

    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 8, "cursor-4");
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(text, "baseline", "本地清空不得删掉基线字符：{text:?}");
    assert_eq!(text.len(), 8);
}

#[test]
fn single_char_docs_keep_both_chars_after_detached_merge() {
    let (_dir, vault) = fresh_vault();
    let remote = memory_doc("doc-1", B_PEER, "a");
    let snapshot = remote.export_snapshot().expect("基线快照");

    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "b").expect("本地单字符");
    commit_now(&vault, &local, "local-batch");

    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 9, "cursor-5");
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(text.len(), 2, "两个单字符必须都在：{text:?}");
    assert_eq!(text.chars().count(), 2);
    assert_eq!(text.matches('a').count(), 1, "基线字符丢失/重复：{text:?}");
    assert_eq!(text.matches('b').count(), 1, "本地字符丢失/重复：{text:?}");
}

#[test]
fn emoji_and_combining_mark_preserved_byte_exact() {
    let (_dir, vault) = fresh_vault();
    let remote = memory_doc("doc-1", B_PEER, "😀");
    let snapshot = remote.export_snapshot().expect("基线快照");

    let local = seed_note(&vault, "note-1", "doc-1");
    local
        .insert_scalars(0, "e\u{301}")
        .expect("本地组合字符（e + U+0301）");
    commit_now(&vault, &local, "local-batch");

    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 10, "cursor-6");
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(
        text.chars().count(),
        3,
        "😀 + e + ◌́ = 3 个 scalar：{text:?}"
    );
    assert_eq!(
        text.len(),
        4 + 3,
        "UTF-8 字节数必须原样保留（😀 4B + e 1B + U+0301 2B）：{text:?}"
    );
    assert_eq!(text.matches("😀").count(), 1, "emoji 丢失/重复：{text:?}");
    assert_eq!(
        text.matches("e\u{301}").count(),
        1,
        "组合字符不得被拆开：{text:?}"
    );
    assert!(
        !text.contains('\u{fffd}'),
        "不得出现替换字符（说明按字节截断过）：{text:?}"
    );
}

#[test]
fn precomposed_vs_decomposed_accent_merges_without_normalization() {
    let (_dir, vault) = fresh_vault();
    // 远端用预组合 é（U+00E9，2B），本地用分解 e + U+0301（3B）：同形不同字节。
    let remote = memory_doc("doc-1", B_PEER, "é");
    let snapshot = remote.export_snapshot().expect("基线快照");

    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "e\u{301}").expect("本地分解形式");
    commit_now(&vault, &local, "local-batch");

    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 11, "cursor-7");
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(text.len(), 2 + 3, "两种编码的字节都要保留：{text:?}");
    assert_eq!(text.chars().count(), 3);
    assert_eq!(text.matches('é').count(), 1, "预组合形式丢失：{text:?}");
    assert_eq!(
        text.matches("e\u{301}").count(),
        1,
        "分解形式丢失：{text:?}"
    );
    assert_ne!(text, "éé", "不得做 NFC 归一化：{text:?}");
    assert_ne!(text, "e\u{301}e\u{301}", "不得做 NFD 归一化：{text:?}");
}
