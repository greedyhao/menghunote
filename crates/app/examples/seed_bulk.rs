//! 压测用的批量播种工具（**不属产品**，只给 UI 压力验证用）。
//!
//! 用法：
//! ```bash
//! cargo run -p menghunote-app --example seed_bulk -- <vault 路径> <笔记数>
//! ```
//!
//! 它用与产品同一条结构写入路径（`StructureWriter` → `commit` → 投影）建 N 篇笔记，
//! 并给每篇写一小段正文，便于验证"导出 200 篇时 UI 是否仍然响应"。

use std::path::PathBuf;

use anyhow::Context;
use menghunote_storage::{load_body_doc, StructureWriter, UpdateRecord, Vault};

const PEER_ID: u64 = 9_000_001;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .map(PathBuf::from)
        .context("用法：seed_bulk <vault 路径> <笔记数>")?;
    let count: usize = args
        .next()
        .context("缺少笔记数")?
        .parse()
        .context("笔记数不是数字")?;

    if path.exists() {
        std::fs::remove_file(&path).ok();
    }
    let vault = Vault::open(&path).context("打开 vault 失败")?;
    let now = 1_i64;
    let mut writer = StructureWriter::open(&vault, PEER_ID).context("打开结构写入口失败")?;
    let mut created = Vec::with_capacity(count);
    for index in 0..count {
        let title = format!("压测笔记 {index:03}");
        let node = writer
            .doc()
            .create_note(&title, now + index as i64)
            .with_context(|| format!("创建笔记失败：{title}"))?;
        created.push(node);
    }
    writer.commit("seed-bulk").context("提交结构失败")?;

    for (index, node) in created.iter().enumerate() {
        let body =
            format!("# 压测笔记 {index:03}\n\n这是第 {index} 篇压测正文，用来撑大导出量。\n");
        let (doc, head) = load_body_doc(&vault, &node.body_doc_id, PEER_ID, None)
            .with_context(|| format!("加载正文失败：{}", node.body_doc_id))?;
        doc.insert_scalars(0, &body).context("写正文失败")?;
        let base = head.version_vector().context("解码 head_vv 失败")?;
        let update = doc.pending_update(&base).context("导出增量失败")?;
        std::fs::create_dir_all(path.parent().unwrap_or(std::path::Path::new("."))).ok();
        menghunote_storage::append_updates(
            &vault,
            &node.body_doc_id,
            head.head_seq,
            &[UpdateRecord::new("seed-bulk", update)],
        )
        .context("写 A 事务失败")?;
    }

    // 额外一篇：6 行都含 `needle`，用来验收"按笔记分组 + 多行摘要 + 共 N 处命中"。
    let mut writer = StructureWriter::open(&vault, PEER_ID).context("重开结构写入口失败")?;
    let multi = writer
        .doc()
        .create_note("多行命中示例", 9_000_000)
        .context("创建多行命中笔记失败")?;
    writer.commit("seed-multiline").context("提交结构失败")?;
    let body = "# 多行命中示例\n\nneedle 第 1 行\nneedle 第 2 行\nneedle 第 3 行\nneedle 第 4 行\nneedle 第 5 行\nneedle 第 6 行\n";
    let (doc, head) =
        load_body_doc(&vault, &multi.body_doc_id, PEER_ID, None).context("加载正文失败")?;
    doc.insert_scalars(0, body).context("写正文失败")?;
    let base = head.version_vector().context("解码 head_vv 失败")?;
    let update = doc.pending_update(&base).context("导出增量失败")?;
    menghunote_storage::append_updates(
        &vault,
        &multi.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("seed-multiline", update)],
    )
    .context("写 A 事务失败")?;

    println!(
        "已播种 {count} 篇笔记到 {}（另加 1 篇多行命中示例）",
        path.display()
    );
    Ok(())
}
