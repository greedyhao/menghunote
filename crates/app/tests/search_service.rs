//! S4-2b 集成测试：后台检索服务对真实 SQLite + Tantivy 的端到端行为。
//!
//! 不启窗口，直接起 [`SearchService`]（它内部就是产品用的那条后台线程），
//! 用 **明确的收敛点**（`recv_timeout` 收到 `Status::Ready` / `Hits`）等待，
//! 不用 sleep 轮询。

use std::path::PathBuf;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use menghunote_app::search_service::{is_latest, SearchReply, SearchService, SearchStatus};
use menghunote_storage::{append_updates, load_body_doc, StructureWriter, UpdateRecord, Vault};

const PEER: u64 = 4_200_001;
const WAIT: Duration = Duration::from_secs(20);

fn tmp_vault(name: &str) -> Vault {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录失败");
    Vault::open(dir.join("vault.sqlite3")).expect("打开 vault 失败")
}

/// 建一篇笔记并把正文写进 A 事务（物化由 storage 的物化驱动完成；
/// 这里直接用 `materialize_body` 把它推到 notes 表，好让索引能读到）。
fn seed(vault: &Vault, title: &str, body: &str, now: i64) -> String {
    let mut writer = StructureWriter::open(vault, PEER).expect("打开结构写入口失败");
    let node = writer.doc().create_note(title, now).expect("创建笔记失败");
    writer.commit("seed").expect("提交结构失败");
    let (doc, head) = load_body_doc(vault, &node.body_doc_id, PEER, None).expect("加载正文失败");
    doc.insert_scalars(0, body).expect("写正文失败");
    let base = head.version_vector().expect("解码 vv 失败");
    let update = doc.pending_update(&base).expect("导出增量失败");
    let new_head = append_updates(
        vault,
        &node.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("seed", update)],
    )
    .expect("写 A 事务失败");
    menghunote_storage::materialize_body(
        vault,
        &node.note_id,
        &node.body_doc_id,
        new_head.head_seq,
        PEER,
    )
    .expect("物化正文失败");
    node.note_id
}

#[test]
fn service_indexes_and_queries_real_notes() {
    let vault = tmp_vault("s4b-service");
    let rust_note = seed(&vault, "Rust 指南", "掌握 rust 的异步与生命周期。", 1);
    let _other = seed(&vault, "无关笔记", "今天天气不错。", 2);
    let chinese_note = seed(
        &vault,
        "压测正文",
        "这是压测用的中文正文，检索要能命中。",
        3,
    );

    let mut service = SearchService::start(vault);

    // ① 等启动对账结束（明确收敛点：Ready 或 Degraded）。
    let mut ready = false;
    let deadline = std::time::Instant::now() + WAIT;
    while std::time::Instant::now() < deadline {
        match service.recv_timeout(Duration::from_millis(500)) {
            Ok(SearchReply::Status(SearchStatus::Ready))
            | Ok(SearchReply::Status(SearchStatus::Degraded(_))) => {
                ready = true;
                break;
            }
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    assert!(ready, "索引启动没有在 {WAIT:?} 内就绪");

    // ② 发查询，等这一条 request_id 的结果（旧 id 一律忽略）。
    let request_id = service.request("rust", 10);
    let mut hits = None;
    while std::time::Instant::now() < deadline {
        match service.recv_timeout(Duration::from_millis(500)) {
            Ok(SearchReply::Hits {
                request_id: got,
                hits: got_hits,
            }) => {
                if is_latest(request_id, got) {
                    hits = Some(got_hits);
                    break;
                }
            }
            Ok(SearchReply::Failed { message, .. }) => panic!("查询失败：{message}"),
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let hits = hits.expect("没有收到查询结果");
    assert!(
        hits.iter().any(|hit| hit.note_id == rust_note),
        "结果里应包含刚建的那篇，实际：{:?}",
        hits.iter().map(|hit| &hit.note_id).collect::<Vec<_>>()
    );
    let hit = hits
        .iter()
        .find(|hit| hit.note_id == rust_note)
        .expect("命中缺失");
    assert!(
        hit.snippet_text.to_lowercase().contains("rust"),
        "snippet 应包含关键词：{}",
        hit.snippet_text
    );
    assert!(
        !hit.snippet_ranges.is_empty(),
        "snippet_ranges 不该为空（UI 靠它做高亮）"
    );
    for range in &hit.snippet_ranges {
        assert!(
            range.end <= hit.snippet_text.len(),
            "高亮区间越界：{range:?} > {}",
            hit.snippet_text.len()
        );
    }

    // ③ 中文查询也要能命中（analyzer 的 CJK 处理是 ADR 0005 的一部分）。
    let chinese_id = service.request("压测", 10);
    let mut chinese_hits = None;
    while std::time::Instant::now() < deadline {
        match service.recv_timeout(Duration::from_millis(500)) {
            Ok(SearchReply::Hits {
                request_id: got,
                hits: got_hits,
            }) => {
                if is_latest(chinese_id, got) {
                    chinese_hits = Some(got_hits);
                    break;
                }
            }
            Ok(SearchReply::Failed { message, .. }) => panic!("中文查询失败：{message}"),
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let chinese_hits = chinese_hits.expect("没有收到中文查询结果");
    assert!(
        chinese_hits.iter().any(|hit| hit.note_id == chinese_note),
        "中文查询应命中压测笔记"
    );
}

#[test]
fn latest_wins_drops_stale_responses() {
    let vault = tmp_vault("s4b-latest");
    let rust = seed(&vault, "Rust 笔记", "rust 语言", 1);
    let python = seed(&vault, "Python 笔记", "python 语言", 2);

    let mut service = SearchService::start(vault);
    // 等就绪
    let deadline = std::time::Instant::now() + WAIT;
    while std::time::Instant::now() < deadline {
        match service.recv_timeout(Duration::from_millis(500)) {
            Ok(SearchReply::Status(SearchStatus::Ready | SearchStatus::Degraded(_))) => break,
            Ok(_) | Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => panic!("后台线程提前退出"),
        }
    }

    // 连发两条：只有后一条（最新 id）允许被采用；debounce 也会合并成最后一次。
    let _first = service.request("rust", 10);
    let second = service.request("python", 10);

    let mut final_hits = None;
    while std::time::Instant::now() < deadline {
        match service.recv_timeout(Duration::from_millis(500)) {
            Ok(SearchReply::Hits { request_id, hits }) => {
                if is_latest(second, request_id) {
                    final_hits = Some(hits);
                    break;
                }
                // 旧 id 的响应：测试里也要像 UI 一样丢弃，不能采信。
            }
            Ok(SearchReply::Failed { message, .. }) => panic!("查询失败：{message}"),
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let hits = final_hits.expect("没有收到最新请求的结果");
    assert!(
        hits.iter().any(|hit| hit.note_id == python),
        "最新一次查询（python）应命中对应笔记"
    );
    assert!(
        !hits.iter().any(|hit| hit.note_id == rust),
        "结果应来自最新请求，不该混入上一条（rust）"
    );
}
