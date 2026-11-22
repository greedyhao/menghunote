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

/// 一次查询请求（UI → 后台）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    /// 单调递增的请求号；响应用它做 latest-wins 判定。
    pub request_id: u64,
    /// 查询串（原样交给 Tantivy 的 query parser）。
    pub query: String,
    /// 最多返回多少条。
    pub limit: usize,
}

/// 索引当前状态（UI 展示用）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SearchStatus {
    /// 正在启动/全量对账（"索引准备中…"）。
    #[default]
    Preparing,
    /// 可用。
    Ready,
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
        /// 命中的笔记。
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
    #[must_use]
    pub fn start(vault: Vault) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<SearchRequest>();
        let (reply_tx, reply_rx) = mpsc::channel::<SearchReply>();
        let worker = std::thread::Builder::new()
            .name("menghunote-search".to_owned())
            .spawn(move || worker_main(vault, request_rx, reply_tx))
            .expect("启动检索线程失败");
        Self {
            requests: request_tx,
            replies: reply_rx,
            _worker: worker,
            latest_request: 0,
        }
    }

    /// 发一条查询（自动分配请求号），返回本次请求号。
    pub fn request(&mut self, query: impl Into<String>, limit: usize) -> u64 {
        self.latest_request += 1;
        let request_id = self.latest_request;
        let _ = self.requests.send(SearchRequest {
            request_id,
            query: query.into(),
            limit,
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

/// 后台线程主体：先对账，再循环消费请求 + 驱动增量。
fn worker_main(vault: Vault, requests: Receiver<SearchRequest>, replies: Sender<SearchReply>) {
    // ① 启动对账（可能较慢，UI 此时显示"索引准备中…"）。
    let _ = replies.send(SearchReply::Status(SearchStatus::Preparing));
    let mut manager = match IndexManager::open(&vault) {
        Ok(manager) => manager,
        Err(error) => {
            let _ = replies.send(SearchReply::Status(SearchStatus::Failed(format!(
                "索引启动失败：{error}"
            ))));
            return;
        }
    };
    let status = match manager.degraded() {
        Some(reason) => SearchStatus::Degraded(reason.to_owned()),
        None => SearchStatus::Ready,
    };
    let _ = replies.send(SearchReply::Status(status));

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

        // ③ 驱动 storage 的增量事件：物化/结构提交 → 索引增量更新。
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

        // ④ 到点就执行最新那条查询。
        if let Some(pending) = queue.take_due(now) {
            match manager.query(&pending.query, pending.limit) {
                Ok(hits) => {
                    let _ = replies.send(SearchReply::Hits {
                        request_id: pending.request_id,
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
