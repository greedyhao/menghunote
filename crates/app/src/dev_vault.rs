//! dev vault：本地演示库（切片 5a 起的"打开即有内容"入口）。
//!
//! 位置：`target/tmp/menghunote-dev/vault.sqlite3`（可用 [`VAULT_ENV`] 覆盖），
//! 遵守 AGENTS §6「不要用 `%TEMP%`」。
//!
//! ## 切片 7b：播种改走**结构 CRDT → 投影**这条生产路径
//!
//! 之前是直接 `create_body_doc` + `create_note_slot` 写 SQL 行（没有结构树）。
//! 侧栏读的是 `list_tree()`（结构投影表 `tree`），所以那种种子在侧栏里是**看不见**的。
//! 现在改为：`StructureWriter::create_note/create_folder/move_node` → `commit`
//! （A 事务 + 投影，顺带建好 `notes` / `loro_docs(body)` 行）→ 再往正文文档写内容。
//!
//! 播种 3 个节点，正好覆盖侧栏要演示的三种形态：
//!
//! ```text
//! menghunote · dev 演示笔记        （根级笔记，含 5000 scalar 超长行）
//! 示例目录                         （根级目录）
//! └─ 目录里的第二篇                （depth = 1，用来验证缩进与切换）
//! ```

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use menghunote_storage::{
    append_updates, list_tree, load_body_doc, StructureWriter, UpdateRecord, Vault,
};

/// 演示笔记标题（根级）。
pub const DEMO_TITLE: &str = "menghunote · dev 演示笔记";
/// 示例目录标题（根级）。
pub const DEMO_FOLDER_TITLE: &str = "示例目录";
/// 目录里的第二篇笔记标题（depth = 1）。
pub const DEMO_SECOND_TITLE: &str = "目录里的第二篇";
/// 本机 peer id（切片 7 只有一个编辑器实例，固定 1 即可）。
pub const DEV_PEER_ID: u64 = 1;
/// ADR 0002 §11.1 的冻结夹具：`é` 是 **分解形式**（`e` + U+0301）。
pub const NFD_FIXTURE: &str = "中😀e\u{301}";
/// 超长行的 scalar 数。
pub const LONG_LINE_CHARS: usize = 5000;
/// 覆盖 dev vault 路径的环境变量。
pub const VAULT_ENV: &str = "MENGHUNOTE_DEV_VAULT";
/// 相对仓库根的默认位置（AGENTS.md §6：不要用 `%TEMP%`）。
const DEFAULT_RELATIVE: &str = "tmp/menghunote-dev/vault.sqlite3";

/// 打开后的 dev vault。
pub struct DevVault {
    /// 已打开（并按需迁移）的 vault。
    pub vault: Vault,
    /// 实际使用的库文件路径。
    pub path: PathBuf,
    /// 本次是否执行了播种（已有结构树时为 `false`）。
    pub seeded: bool,
}

/// 打开 dev vault；结构树为空时播种一次。
pub fn open_dev_vault() -> anyhow::Result<DevVault> {
    open_vault_at(&dev_vault_path())
}

/// 打开指定路径的 dev vault（测试与 `MENGHUNOTE_VAULT` 共用这条路径）。
///
/// 幂等判据是**结构树是否为空**（`list_tree`）：7b 之前的老库只有 `notes`/`loro_docs`
/// 行、没有结构节点，那种演示行会在播种时被清掉并重建（见 [`wipe_legacy_demo_rows`]）。
pub fn open_vault_at(path: &Path) -> anyhow::Result<DevVault> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建 dev vault 目录失败：{}", parent.display()))?;
    }
    let vault =
        Vault::open(path).with_context(|| format!("打开 dev vault 失败：{}", path.display()))?;
    let has_tree = !list_tree(&vault).context("读取结构树失败")?.is_empty();
    let seeded = if has_tree {
        false
    } else {
        seed_demo_tree(&vault)?;
        true
    };
    Ok(DevVault {
        vault,
        path: path.to_path_buf(),
        seeded,
    })
}

/// dev vault 的默认路径（可用 [`VAULT_ENV`] 覆盖）。
///
/// 优先 `CARGO_TARGET_DIR`，否则仓库根下的 `target`。
#[must_use]
pub fn dev_vault_path() -> PathBuf {
    if let Some(overridden) = std::env::var_os(VAULT_ENV) {
        return PathBuf::from(overridden);
    }
    let target = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
        },
        PathBuf::from,
    );
    target.join(DEFAULT_RELATIVE)
}

/// 演示笔记的 Markdown 源码（LF 归一、S1 identity：不做任何标记隐藏）。
#[must_use]
pub fn demo_markdown() -> String {
    let mut text = String::new();
    text.push_str("# menghunote · dev 演示笔记\n\n");
    text.push_str("切片 5a 只做一件事：把 `source_text()` 按**软折行**画进 GPUI 窗口。\n");
    text.push_str("S1 是 identity 投影 —— 源码原样显示，`#`、`**`、反引号都还看得见。\n\n");
    text.push_str("单位夹具（ADR 0002 §11.1）：");
    text.push_str(NFD_FIXTURE);
    text.push_str("\n顺序是：中（1 scalar / 3 bytes）、😀（1 scalar / 4 bytes / 2 UTF-16）、\n");
    text.push_str("e + U+0301（2 scalar / 1 grapheme / 3 bytes）。\n\n");
    text.push_str(&format!(
        "下面是 {LONG_LINE_CHARS} 个 scalar 的超长行，用来验证软折行与视觉行命中：\n\n"
    ));
    text.push_str(&long_line(LONG_LINE_CHARS));
    text.push_str("\n\n");
    text.push_str("这几行在第二屏之后：滚轮、PageDown、点击都能到。\n");
    text.push_str("点这里试试拖拽选区，再按左右方向键 —— 光标按 **grapheme** 走，");
    text.push_str("不会把 `e` 和 U+0301 拆开。\n");
    text
}

/// 目录里第二篇笔记的正文（短，用来验证"切到另一篇文档"）。
#[must_use]
pub fn second_note_markdown() -> String {
    let mut text = String::new();
    text.push_str("# 目录里的第二篇\n\n");
    text.push_str("这篇在「示例目录」下面，侧栏里应该缩进一层。\n\n");
    text.push_str("点侧栏第一篇再点回来，应能看到两次 `关闭排空` 日志。\n");
    text
}

/// 生成一条 `chars` 个 scalar 的超长行（ASCII 单词 + CJK，覆盖两种断行规则）。
fn long_line(chars: usize) -> String {
    const UNIT: &str = "menghunote软折行 ";
    let unit_chars = UNIT.chars().count().max(1);
    let mut out = String::with_capacity(chars * 2);
    let mut written = 0usize;
    while written < chars {
        let take = (chars - written).min(unit_chars);
        out.extend(UNIT.chars().take(take));
        written += take;
    }
    out
}

/// 7b 之前的老演示行（没有结构节点，侧栏读不到）——只删这三处**明确属于 dev 的**行。
///
/// 只按固定 ID 删，不做范围删除；全新库上这些语句都是空操作。
fn wipe_legacy_demo_rows(vault: &Vault) -> anyhow::Result<()> {
    use menghunote_storage::Value;
    const LEGACY_DOC: &str = "dev-doc-1";
    const LEGACY_NOTE: &str = "dev-note-1";
    // 顺序必须**先子后父**：notes / note_tags 引用 loro_docs(doc_id)，
    // 反了会撞 FOREIGN KEY constraint failed（本机实测过一次）。
    vault
        .exec(
            "DELETE FROM note_tags WHERE note_id = ?1",
            &[Value::Text(LEGACY_NOTE.to_owned())],
        )
        .context("清理老演示 note_tags 行失败")?;
    vault
        .exec(
            "DELETE FROM notes WHERE note_id = ?1",
            &[Value::Text(LEGACY_NOTE.to_owned())],
        )
        .context("清理老演示 notes 行失败")?;
    let doc = Value::Text(LEGACY_DOC.to_owned());
    for sql in [
        "DELETE FROM projection_outbox WHERE doc_id = ?1",
        "DELETE FROM loro_updates WHERE doc_id = ?1",
        "DELETE FROM loro_docs WHERE doc_id = ?1",
    ] {
        vault
            .exec(sql, std::slice::from_ref(&doc))
            .with_context(|| format!("清理老演示行失败：{sql}"))?;
    }
    Ok(())
}

/// 播种演示树：结构 op（笔记 / 目录 / 嵌套）→ `commit`（A 事务 + 投影）→ 写正文。
fn seed_demo_tree(vault: &Vault) -> anyhow::Result<()> {
    wipe_legacy_demo_rows(vault)?;

    let now = 1_i64;
    let mut writer = StructureWriter::open(vault, DEV_PEER_ID).context("打开结构写入口失败")?;
    let demo = writer
        .doc()
        .create_note(DEMO_TITLE, now)
        .context("创建演示笔记失败")?;
    let folder = writer
        .doc()
        .create_folder(DEMO_FOLDER_TITLE, now + 1)
        .context("创建示例目录失败")?;
    let second = writer
        .doc()
        .create_note(DEMO_SECOND_TITLE, now + 2)
        .context("创建第二篇笔记失败")?;
    writer
        .doc()
        .move_node(&second.node_id, Some(&folder), now + 3)
        .context("把第二篇笔记挂到示例目录下失败")?;
    // commit：A 事务写入结构增量 + 投影（tree / notes / loro_docs 等行）
    writer.commit("dev-seed").context("提交结构并投影失败")?;

    write_body(vault, &demo.body_doc_id, &demo_markdown())?;
    write_body(vault, &second.body_doc_id, &second_note_markdown())?;
    Ok(())
}

/// 把一段 Markdown 写进**已由投影建好行**的正文文档（A 事务落库）。
///
/// 注意：`loro_docs` 行是结构投影建的，所以这里用 `load_body_doc` 而**不是**
/// `create_body_doc`（后者是裸 INSERT，会撞主键）。
fn write_body(vault: &Vault, doc_id: &str, text: &str) -> anyhow::Result<()> {
    let (doc, head) = load_body_doc(vault, doc_id, DEV_PEER_ID, None)
        .with_context(|| format!("加载正文文档失败：{doc_id}"))?;
    doc.insert_scalars(0, text)
        .with_context(|| format!("写入正文失败：{doc_id}"))?;
    let base = head.version_vector().context("解码 head_vv 失败")?;
    let update = doc
        .pending_update(&base)
        .with_context(|| format!("导出正文增量失败：{doc_id}"))?;
    append_updates(
        vault,
        doc_id,
        head.head_seq,
        &[UpdateRecord::new("dev-seed", update)],
    )
    .with_context(|| format!("A 事务写入正文失败：{doc_id}"))?;
    Ok(())
}
