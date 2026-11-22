//! 后台检索服务（S4-2b）：把 `IndexManager` 放在独立线程里，UI 只发请求、收结果。
//!
//! ## 为什么必须是独立线程
//!
//! `IndexManager::open` 会做一次**全量对账**（扫全库、比对版本、写索引），Tantivy 的
//! writer/reader 也不是 `Sync`。放到 UI 线程上就是"一开搜索界面卡几秒"。所以：
//!
//! - 一个专用线程持有 `IndexManager`，启动先对账，然后进循环；
//! - UI 只通过 channel 发 [`SearchRequest`]，通过另一个 channel 收 [`SearchReply`]；
//! - UI 侧用定时轮询（[`SearchService::poll`]）把结果取回，**从不直接碰索引**。
//!
//! ## debounce 与 latest-wins
//!
//! 每敲一个字就发一次查询会打爆索引。这里的规则：
//!
//! 1. UI 每产生一次输入就发一条新请求（`request_id` 单调递增）；
//! 2. 后台把**最新一条**记为 pending，`due_at = 首次收到的时刻 + DEBOUNCE`；
//! 3. 到点后只执行 pending 里那一条（旧请求被直接丢弃 = latest-wins）；
//! 4. 回传时带上 `request_id`，UI 若发现不是最新 id 就丢弃（防止乱序）。
//!
//! 纯逻辑（[`DebounceQueue`]）与线程解耦，可以用可控时钟单测，不需要 sleep。

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use menghunote_search::{Hit, IndexManager};
use menghunote_storage::Vault;

/// 输入停止多久后才真正查询（毫秒）。
pub const DEBOUNCE_MS: u64 = 180;

/// 后台循环的驱动间隔（毫秒）：驱动物化事件、flush、以及到点的查询。
pub const TICK_MS: u64 = 50;

/// 单次查询返回的最大条数。
pub const DEFAULT_LIMIT: usize = 20;

/// 查询选项（大小写敏感 / 整词）。
///
/// 与 S5-1 的 `menghunote_search::QueryOptions` 同构；放在 app 侧是为了
/// **UI 与后台协议先独立**，等 search crate 的 `query_with_options` 落地后
/// 由 [`query_backend`] 一处转换成它，UI 不需要改。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QueryOptions {
    /// 区分大小写（`Aa` 开关）。
    pub case_sensitive: bool,
    /// 整词匹配（`整词` 开关）；关 = 子串匹配。
    pub whole_word: bool,
}

impl QueryOptions {
    /// 默认：不区分大小写、子串匹配。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            case_sensitive: false,
            whole_word: false,
        }
    }
}

/// 一次查询请求（UI → 后台）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    /// 单调递增的请求号；响应用它做 latest-wins 判定。
    pub request_id: u64,
    /// 查询串（原样交给 Tantivy 的 query parser）。
    pub query: String,
    /// 最多返回多少条。
    pub limit: usize,
    /// 查询选项（大小写/整词）。
    pub options: QueryOptions,
}

/// 索引构建阶段（进度条用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildPhase {
    /// 追赶正文（`reconcile` 物化所有 stale 笔记）。
    Materialize,
    /// 建立索引（新代构建 / 全量对账写 Tantivy）。
    Index,
}

impl BuildPhase {
    /// 阶段中文名（直接显示在浮层状态行）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Materialize => "补齐正文",
            Self::Index => "建立索引",
        }
    }
}

/// 进度百分比：`total == 0`（未知）时返回 `None`，UI 走不确定态。
///
/// **不要显示 0%**：总数未知时 0% 会让人以为卡死了。
#[must_use]
pub fn progress_percent(done: usize, total: usize) -> Option<u32> {
    if total == 0 {
        return None;
    }
    let clamped = done.min(total);
    Some(((clamped as u64 * 100) / total as u64) as u32)
}

/// 索引当前状态（UI 展示用）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SearchStatus {
    /// 正在启动/全量对账（"索引准备中…"）。
    #[default]
    Preparing,
    /// 可用。
    Ready,
    /// 正在构建（首次建索引 / 重建）：带确定性进度或未知总量。
    Progress {
        /// 当前阶段。
        phase: BuildPhase,
        /// 已完成数量。
        done: usize,
        /// 总量（`0` = 未知，UI 走不确定态）。
        total: usize,
    },
    /// 降级：能查，但某些笔记没进索引（提示用户）。
    Degraded(String),
    /// 启动失败：永久不可用，直到重启。
    Failed(String),
}

/// 后台回传的一条消息。
#[derive(Debug)]
pub enum SearchReply {
    /// 索引状态变化。
    Status(SearchStatus),
    /// 一次查询的结果（`request_id` 用于丢弃过期响应）。
    Hits {
        /// 请求号。
        request_id: u64,
        /// 命中总数（可能大于 `hits.len()`：`hits` 受 limit 限制）。
        total: usize,
        /// 命中的笔记（最多 `limit` 条）。
        hits: Vec<Hit>,
    },
    /// 一次查询失败（可诊断信息）。
    Failed {
        /// 请求号。
        request_id: u64,
        /// 失败原因。
        message: String,
    },
}

/// 后台的待执行查询队列（纯逻辑，便于用可控时钟单测）。
#[derive(Debug, Default)]
pub struct DebounceQueue {
    pending: Option<Pending>,
}

/// 等待执行的查询。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// 请求号。
    pub request_id: u64,
    /// 查询串。
    pub query: String,
    /// 条数上限。
    pub limit: usize,
    /// 查询选项。
    pub options: QueryOptions,
    /// 到点时间。
    pub due_at: Instant,
}

impl DebounceQueue {
    /// 提交一条新请求。
    ///
    /// **latest-wins**：新请求直接覆盖旧的，并且**重新计时**（用户还在打字就别查）。
    pub fn submit(&mut self, request: SearchRequest, now: Instant) {
        self.pending = Some(Pending {
            request_id: request.request_id,
            query: request.query,
            limit: request.limit,
            options: request.options,
            due_at: now + Duration::from_millis(DEBOUNCE_MS),
        });
    }

    /// 取走到点的那条（没有到点就返回 `None`，队列保持不动）。
    pub fn take_due(&mut self, now: Instant) -> Option<Pending> {
        match &self.pending {
            Some(pending) if now >= pending.due_at => self.pending.take(),
            _ => None,
        }
    }

    /// 当前是否有待执行请求。
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
}

/// UI 侧"最新请求号"判定：只有等于最新的响应才允许上屏。
#[must_use]
pub fn is_latest(latest: u64, reply: u64) -> bool {
    latest == reply
}

/// 后台检索服务句柄（UI 侧持有）。
pub struct SearchService {
    /// 发请求。
    requests: Sender<SearchRequest>,
    /// 收结果。
    replies: Receiver<SearchReply>,
    /// 后台线程句柄（Drop 时 detach，不让 UI 卡在 join 上）。
    _worker: std::thread::JoinHandle<()>,
    /// 最近一次请求号。
    latest_request: u64,
}

impl SearchService {
    /// 启动后台线程并开始对账（立即返回，UI 不会阻塞）。
    ///
    /// `peer_id` 用于物化（写 B 投影时要带 peer），由宿主传入，storage/search 都不硬编码。
    #[must_use]
    pub fn start(vault: Vault, peer_id: u64) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<SearchRequest>();
        let (reply_tx, reply_rx) = mpsc::channel::<SearchReply>();
        let worker = std::thread::Builder::new()
            .name("menghunote-search".to_owned())
            .spawn(move || worker_main(vault, peer_id, request_rx, reply_tx))
            .expect("启动检索线程失败");
        Self {
            requests: request_tx,
            replies: reply_rx,
            _worker: worker,
            latest_request: 0,
        }
    }

    /// 发一条查询（自动分配请求号），返回本次请求号。
    pub fn request(
        &mut self,
        query: impl Into<String>,
        limit: usize,
        options: QueryOptions,
    ) -> u64 {
        self.latest_request += 1;
        let request_id = self.latest_request;
        let _ = self.requests.send(SearchRequest {
            request_id,
            query: query.into(),
            limit,
            options,
        });
        request_id
    }

    /// 最近一次请求号（UI 用它丢弃过期响应）。
    #[must_use]
    pub fn latest_request(&self) -> u64 {
        self.latest_request
    }

    /// 阻塞等待一条后台消息（**测试用**：等明确收敛点，不靠 sleep 轮询）。
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<SearchReply, std::sync::mpsc::RecvTimeoutError> {
        self.replies.recv_timeout(timeout)
    }

    /// 非阻塞取走后台回传的所有消息。
    pub fn poll(&self) -> Vec<SearchReply> {
        let mut out = Vec::new();
        while let Ok(reply) = self.replies.try_recv() {
            out.push(reply);
        }
        out
    }
}

/// 周期对账间隔（ADR 0005：事件是主路径，周期对账兜底漏事件/进程外改动）。
pub const PERIODIC_RECONCILE: Duration = Duration::from_secs(30);

/// 查询适配器：app 的 [`QueryOptions`] → search crate 的 `QueryOptions` 并查询。
///
/// 两边的 `whole_word` 语义一致：`true` = 整词（精确 token），`false` = 子串
/// （trigram 召回 + 原文校验）。`total` 是索引侧命中数，`hits` 最多 `limit` 条。
///
/// 返回 `(总数, 命中列表)`。
fn query_backend(
    manager: &IndexManager,
    query: &str,
    limit: usize,
    options: QueryOptions,
) -> Result<(usize, Vec<Hit>), menghunote_search::SearchError> {
    let result = manager.query_with_options(
        query,
        limit,
        menghunote_search::QueryOptions {
            case_sensitive: options.case_sensitive,
            whole_word: options.whole_word,
        },
    )?;
    Ok((result.total, result.hits))
}

/// 物化所有 stale 正文，并在过程中上报「补齐正文 done/total」进度。
///
/// 逐篇物化（而不是一次性 `reconcile`）是为了给 UI 确定性进度；`total` 先扫一遍
/// stale 列表拿到，扫不到（0 篇）时直接结束。
fn reconcile_bodies_with_progress(
    vault: &Vault,
    peer_id: u64,
    replies: &Sender<SearchReply>,
) -> Result<usize, String> {
    let started = Instant::now();
    let stale = menghunote_storage::scan_stale_bodies(vault)
        .map_err(|error| format!("扫描待物化正文失败：{error}"))?;
    let total = stale.len();
    let _ = replies.send(SearchReply::Status(SearchStatus::Progress {
        phase: BuildPhase::Materialize,
        done: 0,
        total,
    }));
    let mut applied = 0usize;
    for (index, item) in stale.iter().enumerate() {
        match menghunote_storage::materialize_body(
            vault,
            &item.note_id,
            &item.doc_id,
            item.head_seq,
            peer_id,
        ) {
            Ok(menghunote_storage::MaterializeOutcome::Applied { .. }) => applied += 1,
            Ok(menghunote_storage::MaterializeOutcome::Stale { .. }) => {}
            Err(error) => return Err(format!("正文物化失败：{error}")),
        }
        // 按篇上报；UI 侧一次 drain 只 notify 一帧，所以不会每篇刷一次屏。
        let _ = replies.send(SearchReply::Status(SearchStatus::Progress {
            phase: BuildPhase::Materialize,
            done: index + 1,
            total,
        }));
    }
    eprintln!(
        "[menghunote] 搜索：reconcile 完成，物化 {applied} 篇 / 检查 {total} 篇（{} ms）",
        started.elapsed().as_millis()
    );
    Ok(applied)
}

/// 物化所有 stale 正文，返回**实际写入**的篇数（`Stale` 不计）。
fn reconcile_bodies(vault: &Vault, peer_id: u64) -> Result<usize, String> {
    let started = Instant::now();
    let outcomes = menghunote_storage::reconcile(vault, peer_id)
        .map_err(|error| format!("正文物化失败：{error}"))?;
    let applied = outcomes
        .iter()
        .filter(|outcome| {
            matches!(
                outcome,
                menghunote_storage::MaterializeOutcome::Applied { .. }
            )
        })
        .count();
    eprintln!(
        "[menghunote] 搜索：reconcile 完成，物化 {applied} 篇 / 检查 {} 篇（{} ms）",
        outcomes.len(),
        started.elapsed().as_millis()
    );
    Ok(applied)
}

/// 后台线程主体：先对账，再循环消费请求 + 驱动增量。
fn worker_main(
    vault: Vault,
    peer_id: u64,
    requests: Receiver<SearchRequest>,
    replies: Sender<SearchReply>,
) {
    // ① 启动对账（可能较慢，UI 此时显示"索引准备中…"）。
    let _ = replies.send(SearchReply::Status(SearchStatus::Preparing));

    // ①-0 **先把 stale 正文物化，再建索引**（P0）。
    //
    // 索引是从 `notes.body_md` 建的（ADR 0005），而 `body_md` 只在 B 物化时写。
    // 新库/播种库只有 A 正文 CRDT 时 `body_md` 是 NULL → 索引永远是空代 → 搜索无结果。
    // 所以进入索引前必须 `reconcile`：把所有 body_seq 落后 / frontier 不一致 / 从未
    // 物化过的正文补出来。这一步在启动时做一次，之后每 [`PERIODIC_RECONCILE`] 再做。
    let mut problems: Vec<String> = Vec::new();
    match reconcile_bodies_with_progress(&vault, peer_id, &replies) {
        Ok(applied) => {
            if applied > 0 {
                eprintln!("[menghunote] 搜索：启动物化 {applied} 篇正文");
            }
        }
        Err(message) => problems.push(message),
    }

    // 首次建索引/重建会回调 `BuildProgress`（每 50 篇 + 首尾）；复用已有代时不回调。
    let progress_replies = replies.clone();
    let mut manager = match IndexManager::open_with_progress(&vault, move |progress| {
        // search 目前只报建索引阶段；物化阶段由 app 自己报（见
        // `reconcile_bodies_with_progress`）。将来 search 若也报物化，这里直接映射。
        let phase = match progress.phase {
            menghunote_search::ProgressPhase::MaterializeBodies => BuildPhase::Materialize,
            menghunote_search::ProgressPhase::BuildIndex => BuildPhase::Index,
        };
        let _ = progress_replies.send(SearchReply::Status(SearchStatus::Progress {
            phase,
            done: progress.done,
            total: progress.total,
        }));
    }) {
        Ok(manager) => manager,
        Err(error) => {
            let _ = replies.send(SearchReply::Status(SearchStatus::Failed(format!(
                "索引启动失败：{error}"
            ))));
            return;
        }
    };
    if let Some(reason) = manager.degraded() {
        problems.push(format!("索引降级：{reason}"));
    }
    let status = if problems.is_empty() {
        SearchStatus::Ready
    } else {
        SearchStatus::Degraded(problems.join("；"))
    };
    let _ = replies.send(SearchReply::Status(status));

    let mut last_periodic = Instant::now();

    let mut queue = DebounceQueue::default();
    loop {
        let now = Instant::now();

        // ② 把 UI 侧发来的请求全部收下（只保留最新一条）。
        loop {
            match requests.try_recv() {
                Ok(request) => queue.submit(request, now),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        // ③ 周期对账（处理漏掉的事件 / 进程外改动）：先补物化，再让索引全量对账。
        if now.duration_since(last_periodic) >= PERIODIC_RECONCILE {
            last_periodic = now;
            let applied = reconcile_bodies(&vault, peer_id).unwrap_or_else(|message| {
                eprintln!("[menghunote] 搜索：周期物化失败：{message}");
                0
            });
            if applied > 0 {
                eprintln!("[menghunote] 搜索：周期物化 {applied} 篇正文");
            }
            if let Err(error) = manager.reconcile_now() {
                eprintln!("[menghunote] 搜索：周期索引对账失败：{error}");
            }
        }

        // ④ 驱动 storage 的增量事件：物化/结构提交 → 索引增量更新。
        //    事件送不到时 storage 会置 reconcile_required，这里补一次全量对账。
        if vault.take_reconcile_required() {
            if let Err(error) = manager.reconcile_now() {
                let _ = replies.send(SearchReply::Status(SearchStatus::Failed(format!(
                    "索引对账失败：{error}"
                ))));
            }
        }
        let _ = manager.poll_events(now);
        let _ = manager.flush_if_due(now);

        // ⑤ 到点就执行最新那条查询。
        if let Some(pending) = queue.take_due(now) {
            match query_backend(&manager, &pending.query, pending.limit, pending.options) {
                Ok((total, hits)) => {
                    let _ = replies.send(SearchReply::Hits {
                        request_id: pending.request_id,
                        total,
                        hits,
                    });
                }
                Err(error) => {
                    let _ = replies.send(SearchReply::Failed {
                        request_id: pending.request_id,
                        message: format!("查询失败：{error}"),
                    });
                }
            }
        }

        std::thread::sleep(Duration::from_millis(TICK_MS));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: u64, query: &str) -> SearchRequest {
        SearchRequest {
            request_id: id,
            query: query.to_owned(),
            limit: 10,
            options: QueryOptions::new(),
        }
    }

    #[test]
    fn debounce_fires_only_after_the_window() {
        let mut queue = DebounceQueue::default();
        let now = Instant::now();
        queue.submit(request(1, "a"), now);

        assert!(queue.take_due(now).is_none(), "刚提交不应立即执行");
        let almost = now + Duration::from_millis(DEBOUNCE_MS - 1);
        assert!(queue.take_due(almost).is_none(), "未到 debounce 窗口");
        let due = now + Duration::from_millis(DEBOUNCE_MS);
        let pending = queue.take_due(due).expect("到点应返回");
        assert_eq!(pending.request_id, 1);
        assert!(!queue.is_pending(), "取走后队列应清空");
    }

    #[test]
    fn latest_request_wins_and_restarts_the_timer() {
        let mut queue = DebounceQueue::default();
        let now = Instant::now();
        queue.submit(request(1, "a"), now);
        // 100ms 后又敲了一个字：旧请求被丢弃，重新计时
        let later = now + Duration::from_millis(100);
        queue.submit(request(2, "ab"), later);

        let old_due = now + Duration::from_millis(DEBOUNCE_MS);
        assert!(queue.take_due(old_due).is_none(), "旧请求应已被覆盖");

        let new_due = later + Duration::from_millis(DEBOUNCE_MS);
        let pending = queue.take_due(new_due).expect("新请求到点应执行");
        assert_eq!(pending.request_id, 2);
        assert_eq!(pending.query, "ab", "执行的必须是最后那次输入");
    }

    #[test]
    fn stale_replies_are_rejected() {
        assert!(is_latest(7, 7));
        assert!(!is_latest(7, 6), "旧 request_id 的响应必须丢弃");
        assert!(!is_latest(7, 8), "未来 id 也不该当作最新");
    }
}
