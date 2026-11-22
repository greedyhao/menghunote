//! dev vault 的播种 / 重放（AGENTS.md §6：临时目录一律用 `CARGO_TARGET_TMPDIR`，
//! **不要**用 `%TEMP%`，本机的 cargo 子进程不可写）。
//!
//! 切片 7b 起播种走**结构 CRDT → 投影**：`list_tree` 能读到 "演示笔记 / 示例目录 /
//! 目录里的第二篇" 三个节点，侧栏与多文档切换才有东西可点。

use std::path::Path;

use menghunote_app::dev_vault::{
    self, DEMO_FOLDER_TITLE, DEMO_SECOND_TITLE, DEMO_TITLE, DEV_PEER_ID, LONG_LINE_CHARS,
    NFD_FIXTURE,
};
use menghunote_editor::EditorController;
use menghunote_storage::session::{DriverOptions, SessionDriver};
use menghunote_storage::{list_tree, load_body_doc, StructureWriter};

fn temp_vault(name: &str) -> std::path::PathBuf {
    let base = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("menghunote-app-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("创建 target/tmp 下的测试目录");
    base.join(name)
}

#[test]
fn dev_vault_seeds_tree_and_editor_round_trips_it() {
    let path = temp_vault("seed-once.sqlite3");
    let _ = std::fs::remove_file(&path);

    // 1. 空库：必须播种
    let dev = dev_vault::open_vault_at(&path).expect("打开 dev vault");
    assert!(dev.seeded, "空库必须播种演示树");

    // 2. 结构树：先序 DFS 三项，目录里的第二篇 depth = 1
    let tree = list_tree(&dev.vault).expect("读结构树");
    let shape: Vec<(&str, usize, bool)> = tree
        .iter()
        .map(|entry| (entry.title.as_str(), entry.depth, entry.is_folder()))
        .collect();
    assert_eq!(
        shape,
        vec![
            (DEMO_TITLE, 0, false),
            (DEMO_FOLDER_TITLE, 0, true),
            (DEMO_SECOND_TITLE, 1, false),
        ],
        "侧栏顺序/深度必须与播种一致"
    );
    assert!(tree[0].body_doc_id.is_some(), "笔记必须带 body_doc_id");
    assert!(
        tree[1].body_doc_id.is_none() && tree[1].note_id.is_none(),
        "目录没有正文与 note_id"
    );

    // 3. 经 DocumentSession 重放第一篇：文本来自库里的 CRDT 真源
    let demo_doc = tree[0].body_doc_id.clone().expect("第一篇笔记有正文文档");
    let driver = SessionDriver::open(&dev.vault, &demo_doc, DriverOptions::default())
        .expect("打开第一篇会话");
    let markdown = driver.markdown();
    assert!(
        markdown.contains(NFD_FIXTURE),
        "演示笔记必须包含冻结夹具 {NFD_FIXTURE:?}"
    );
    let editor = EditorController::open(driver.session().snapshot()).expect("打开编辑器模型");
    assert_eq!(
        editor.source_text(),
        markdown,
        "编辑器镜像必须与会话正文一致"
    );

    // 4. 超长行存在且长度严格等于 LONG_LINE_CHARS（scalar 计）
    let long_line = markdown
        .lines()
        .find(|line| line.chars().count() == LONG_LINE_CHARS)
        .expect("必须有一条 5000 scalar 的超长行");
    assert!(
        long_line.starts_with("menghunote软折行"),
        "超长行内容不符合预期"
    );
    drop(driver);

    // 5. 第二篇也能独立打开（多文档切换的目标存在）
    let second_doc = tree[2].body_doc_id.clone().expect("第二篇笔记有正文文档");
    let second = SessionDriver::open(&dev.vault, &second_doc, DriverOptions::default())
        .expect("打开第二篇会话");
    assert!(
        second.markdown().contains(DEMO_SECOND_TITLE),
        "第二篇正文应包含自己的标题"
    );
    drop(second);

    // 6. 二次打开：幂等，不重复播种，树不变
    let again = dev_vault::open_vault_at(&path).expect("重开 dev vault");
    assert!(!again.seeded, "已有结构树不应重复播种");
    let again_tree = list_tree(&again.vault).expect("重开读树");
    assert_eq!(again_tree.len(), 3, "重开后结构树项数不变");
}

/// ADR 0009 切片 2a：`open_vault_at_with_peer` 的播种必须真的用传入的 peer。
///
/// 断言不看结构体字段，而是读**落库的 CRDT 版本向量**（`VersionVector` 的 peer 键）：
/// 播种的结构 op 与正文增量都必须出现显式 peer，且不得再出现默认常量 1。
#[test]
fn open_vault_at_with_peer_seeds_with_the_given_peer() {
    const PEER: u64 = 0x0123_4567_89ab_cdef;
    let path = temp_vault("peer-seed.sqlite3");
    let _ = std::fs::remove_file(&path);

    let dev = dev_vault::open_vault_at_with_peer(&path, PEER).expect("按显式 peer 打开 dev vault");
    assert!(dev.seeded, "空库必须播种演示树");
    let tree = list_tree(&dev.vault).expect("读结构树");
    assert_eq!(tree.len(), 3, "播种后必须仍是 3 个演示节点");
    let doc_id = tree[0]
        .body_doc_id
        .clone()
        .expect("第一篇笔记必须有 body_doc_id");

    // 结构 doc：播种的结构 op（建节点/移动）必须带显式 peer。
    let writer = StructureWriter::open(&dev.vault, PEER).expect("打开结构写入口");
    let structure_peers: Vec<u64> = writer
        .head()
        .version_vector()
        .expect("解码结构 head_vv")
        .keys()
        .copied()
        .collect();
    assert!(
        structure_peers.contains(&PEER),
        "结构 op 必须由显式 peer 产生，实际 VV peers = {structure_peers:?}"
    );
    assert!(
        !structure_peers.contains(&DEV_PEER_ID),
        "播种不得再落常量 1 的 op，实际 VV peers = {structure_peers:?}"
    );
    drop(writer);

    // 正文 doc：正文增量同样带显式 peer，正文内容正常可读。
    let (doc, head) = load_body_doc(&dev.vault, &doc_id, PEER, None).expect("重放第一篇正文");
    assert!(doc.text().contains(NFD_FIXTURE), "正文必须包含冻结夹具");
    let body_peers: Vec<u64> = head
        .version_vector()
        .expect("解码正文 head_vv")
        .keys()
        .copied()
        .collect();
    assert!(
        body_peers.contains(&PEER),
        "正文增量必须由显式 peer 产生，实际 VV peers = {body_peers:?}"
    );
    assert!(
        !body_peers.contains(&DEV_PEER_ID),
        "正文播种不得再落常量 1 的 op，实际 VV peers = {body_peers:?}"
    );
    drop(doc);

    // 默认入口（peer = 1）重开：不重复播种、树不变——默认行为不受影响。
    let again = dev_vault::open_vault_at(&path).expect("默认路径重开");
    assert!(!again.seeded, "已有结构树不应重复播种");
    assert_eq!(list_tree(&again.vault).expect("重开读树").len(), 3);
}
