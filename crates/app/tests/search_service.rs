//! S4-2b 集成测试：后台检索服务对真实 SQLite + Tantivy 的端到端行为。
//!
//! 不启窗口，直接起 [`SearchService`]（它内部就是产品用的那条后台线程），
//! 用 **明确的收敛点**（`recv_timeout` 收到 `Status::Ready` / `Hits`）等待，
//! 不用 sleep 轮询。

use std::path::PathBuf;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use menghunote_app::search_service::{
    is_latest, QueryOptions, SearchReply, SearchService, SearchStatus,
};
use menghunote_storage::{append_updates, load_body_doc, StructureWriter, UpdateRecord, Vault};

const PEER: u64 = 4_200_001;
const WAIT: Duration = Duration::from_secs(20);

fn tmp_vault(name: &str) -> Vault {
    let dir =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
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

/// 只写结构 + 正文 A 事务，**不调用 materialize**：`notes.body_md` 保持 NULL。
///
/// 这正是用户现场的样子（播种/新库只落了 A，B 物化还没跑），用来复现
/// "搜索永远空"的 P0：索引从 `body_md` 建，NULL 就没有文档可索引。
fn seed_unmaterialized(vault: &Vault, title: &str, body: &str, now: i64) -> String {
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
    node.note_id
}

#[test]
fn service_materializes_unmaterialized_bodies_before_indexing() {
    let vault = tmp_vault("s4c-unmaterialized");
    let note_id = seed_unmaterialized(&vault, "没物化的笔记", "未物化正文 uniqueanchor", 1);

    // 前置断言：body_md 确实是 NULL（否则这个用例复现不了现场）
    let snapshot = vault.open_read_snapshot().expect("打开快照失败");
    let manifest = menghunote_storage::list_index_manifest(&snapshot).expect("读清单失败");
    let row = manifest
        .iter()
        .find(|row| row.note_id == note_id)
        .expect("清单里应有这篇");
    assert!(
        row.body_md.is_none(),
        "用例前提：body_md 必须是 NULL（未物化）"
    );
    drop(snapshot);

    let mut service = SearchService::start(vault, PEER);
    let deadline = std::time::Instant::now() + WAIT;
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        match service.recv_timeout(Duration::from_millis(500)) {
            Ok(SearchReply::Status(SearchStatus::Ready | SearchStatus::Degraded(_))) => {
                ready = true;
                break;
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    assert!(ready, "索引应就绪");

    let request_id = service.request("uniqueanchor", 10, QueryOptions::default());
    let mut hits = None;
    while std::time::Instant::now() < deadline {
        match service.recv_timeout(Duration::from_millis(500)) {
            Ok(SearchReply::Hits {
                request_id: got,
                hits: got_hits,
                ..
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
        hits.iter().any(|hit| hit.note_id == note_id),
        "未物化的正文也必须能被搜到（服务启动时应先 reconcile 物化）"
    );
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

    let mut service = SearchService::start(vault, PEER);

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
    let request_id = service.request("rust", 10, QueryOptions::default());
    let mut hits = None;
    while std::time::Instant::now() < deadline {
        match service.recv_timeout(Duration::from_millis(500)) {
            Ok(SearchReply::Hits {
                request_id: got,
                hits: got_hits,
                ..
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
    assert!(!hit.snippets.is_empty(), "至少应有一条命中行");
    assert!(
        hit.snippets
            .iter()
            .any(|line| line.text.to_lowercase().contains("rust")),
        "命中行里应包含关键词：{:?}",
        hit.snippets.iter().map(|l| &l.text).collect::<Vec<_>>()
    );
    for line in &hit.snippets {
        assert!(
            !line.ranges.is_empty(),
            "每行高亮区间不该为空（UI 靠它做高亮）"
        );
        for range in &line.ranges {
            assert!(
                range.end <= line.text.len(),
                "高亮区间越界：{range:?} > {}",
                line.text.len()
            );
        }
    }

    // ③ 中文查询也要能命中（analyzer 的 CJK 处理是 ADR 0005 的一部分）。
    let chinese_id = service.request("压测", 10, QueryOptions::default());
    let mut chinese_hits = None;
    while std::time::Instant::now() < deadline {
        match service.recv_timeout(Duration::from_millis(500)) {
            Ok(SearchReply::Hits {
                request_id: got,
                hits: got_hits,
                ..
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

    let mut service = SearchService::start(vault, PEER);
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
    let _first = service.request("rust", 10, QueryOptions::default());
    let second = service.request("python", 10, QueryOptions::default());

    let mut final_hits = None;
    while std::time::Instant::now() < deadline {
        match service.recv_timeout(Duration::from_millis(500)) {
            Ok(SearchReply::Hits {
                request_id, hits, ..
            }) => {
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

/// 单行摘要 + 开关语义：子串模式命中 `address`，整词模式只命中 `addr`。
///
/// 依赖 S5-1 的 `query_with_options`（`whole_word: false` = 子串）与 `QueryResult::total`。
#[test]
fn query_options_change_hits_and_total() {
    let vault = tmp_vault("s5-options");
    let exact = seed(&vault, "addr 配置", "addr 是一个地址字段。", 1);
    let longer = seed(&vault, "address 说明", "address 是完整地址。", 2);

    let mut service = SearchService::start(vault, PEER);
    wait_ready(&mut service);

    // 子串模式（默认 options：whole_word = false）：两篇都命中。
    let substring_id = service.request("addr", 10, QueryOptions::default());
    let (total_sub, hits_sub) = wait_hits(&mut service, substring_id);
    assert_eq!(total_sub, 2, "子串模式应命中 addr + address");
    let ids: Vec<&str> = hits_sub.iter().map(|hit| hit.note_id.as_str()).collect();
    assert!(ids.contains(&exact.as_str()) && ids.contains(&longer.as_str()));

    // 整词模式：只有独立词 `addr` 命中。
    let whole = QueryOptions {
        case_sensitive: false,
        whole_word: true,
    };
    let whole_id = service.request("addr", 10, whole);
    let (total_whole, hits_whole) = wait_hits(&mut service, whole_id);
    assert_eq!(total_whole, 1, "整词模式应只剩独立词 addr");
    assert_eq!(hits_whole.len(), 1);
    assert_eq!(hits_whole[0].note_id, exact);
    for line in &hits_whole[0].snippets {
        assert!(
            !line.text.contains('\n'),
            "命中行必须是单行（换行会让 UI 撑成两行）"
        );
    }

    // 大小写敏感：大写查询在不敏感模式下命中，敏感模式下 0 条。
    let insensitive = service.request("ADDR", 10, QueryOptions::default());
    let (total_insensitive, _) = wait_hits(&mut service, insensitive);
    assert_eq!(total_insensitive, 2, "大小写不敏感时 ADDR 应命中两篇");

    let sensitive_options = QueryOptions {
        case_sensitive: true,
        whole_word: false,
    };
    let sensitive = service.request("ADDR", 10, sensitive_options);
    let (total_sensitive, hits_sensitive) = wait_hits(&mut service, sensitive);
    assert_eq!(
        total_sensitive, 0,
        "大小写敏感时 ADDR 不该命中 addr/address"
    );
    assert!(hits_sensitive.is_empty());
}

/// 等到索引就绪（明确收敛点，不用 sleep 轮询计数）。
fn wait_ready(service: &mut SearchService) {
    let deadline = std::time::Instant::now() + WAIT;
    while std::time::Instant::now() < deadline {
        match service.recv_timeout(Duration::from_millis(500)) {
            Ok(SearchReply::Status(SearchStatus::Ready | SearchStatus::Degraded(_))) => return,
            Ok(_) | Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => panic!("后台线程提前退出"),
        }
    }
    panic!("索引没有在 {WAIT:?} 内就绪");
}

/// 等到某次请求的结果，返回 `(total, hits)`。
fn wait_hits(service: &mut SearchService, request_id: u64) -> (usize, Vec<menghunote_search::Hit>) {
    let deadline = std::time::Instant::now() + WAIT;
    while std::time::Instant::now() < deadline {
        match service.recv_timeout(Duration::from_millis(500)) {
            Ok(SearchReply::Hits {
                request_id: got,
                total,
                hits,
            }) => {
                if is_latest(request_id, got) {
                    return (total, hits);
                }
            }
            Ok(SearchReply::Failed { message, .. }) => panic!("查询失败：{message}"),
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    panic!("没有收到查询结果");
}

/// S5-5 之后：一篇笔记的多处命中**全部**返回（不再截断），`matched_lines` 与之一致。
#[test]
fn multiline_hits_return_every_line() {
    let vault = tmp_vault("s5-multiline");
    // 同一篇笔记里 5 行都有 "needle"（不同行号），另有 1 篇只有 1 处。
    let many = seed(
        &vault,
        "多行命中",
        "第一行 needle\n第二行 needle\n第三行 needle\n第四行 needle\n第五行 needle\n",
        1,
    );
    let single = seed(&vault, "单行命中", "只有一处 needle", 2);

    let mut service = SearchService::start(vault, PEER);
    wait_ready(&mut service);

    let request_id = service.request("needle", 10, QueryOptions::default());
    let (_total, hits) = wait_hits(&mut service, request_id);

    let many_hit = hits
        .iter()
        .find(|hit| hit.note_id == many)
        .expect("多行命中的笔记应在结果里");
    assert_eq!(
        many_hit.matched_lines, 5,
        "5 行都命中时 matched_lines 应为 5"
    );
    assert_eq!(
        many_hit.snippets.len(),
        many_hit.matched_lines,
        "命中行应**全部**返回（折叠只发生在 UI 层）"
    );
    // 每行都是单行文本（UI 按单行省略渲染）。
    for line in &many_hit.snippets {
        assert!(!line.text.contains('\n'), "命中行不能含换行");
    }

    let single_hit = hits
        .iter()
        .find(|hit| hit.note_id == single)
        .expect("单行命中的笔记应在结果里");
    assert_eq!(single_hit.matched_lines, 1, "只有一处时统计为 1");
    assert_eq!(single_hit.snippets.len(), 1);
}
